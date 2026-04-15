//! User struct for managing multiple groups.
//!
//! This is the main entry point for the application layer,
//! managing multiple `Group`s and coordinating operations.

use alloy::signers::local::PrivateKeySigner;
use std::sync::Mutex;
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::sync::RwLock;
use tracing::{error, info};

use hashgraph_like_consensus::{
    api::ConsensusServiceAPI, service::DefaultConsensusService, types::ConsensusEvent,
};

use crate::app::consensus::{
    cast_vote, forward_incoming_proposal, forward_incoming_vote, start_voting,
};
use crate::app::error::UserError;
use crate::app::peer_scoring::{
    FixedScoringProvider, InMemoryPeerScoreStorage, PeerScoringService,
};
use crate::app::state_machine::{
    FreezeTimeoutStatus, GroupConfig, GroupState, GroupStateMachine, StateChangeHandler,
};
use crate::core::{
    self, DeMlsProvider, DefaultProvider, FreezeFinalizeResult, Group, GroupEventHandler,
    ProcessResult, ProtocolConfig, ScoreEvent, ScoringConfig, StewardList, create_commit_candidate,
};
use crate::ds::InboundPacket;
use crate::mls_crypto::{
    MemoryDeMlsStorage, MlsService, format_wallet_address, parse_wallet_to_bytes,
};
use prost::Message;

use crate::protos::de_mls::messages::v1::{
    AppMessage, BanRequest, ConversationMessage, GroupUpdateRequest, RemoveMember,
    StewardElectionProposal, StewardListSync, ViolationEvidence, group_update_request,
};

/// Internal state for a group managed by User.
struct GroupEntry {
    group: Group,
    state_machine: GroupStateMachine,
    /// Per-instance UUID embedded in outbound packets for echo-dedup on pub/sub networks.
    /// Generated once at group creation/join; lives here rather than in `Group`
    /// because it is a transport concern, not a protocol concept.
    app_id: Vec<u8>,
}

/// User manages multiple MLS groups.
pub struct User<P: DeMlsProvider, H: GroupEventHandler, SCH: StateChangeHandler> {
    mls_service: MlsService<P::Storage>,
    groups: Arc<RwLock<HashMap<String, GroupEntry>>>,
    consensus_service: Arc<P::Consensus>,
    eth_signer: PrivateKeySigner,
    handler: Arc<H>,
    state_handler: Arc<SCH>,
    default_group_config: GroupConfig,
    scoring_service: Mutex<PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider>>,
}

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    fn new_with_config(
        mls_service: MlsService<P::Storage>,
        consensus_service: Arc<P::Consensus>,
        eth_signer: PrivateKeySigner,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
        default_group_config: GroupConfig,
    ) -> Self {
        Self {
            mls_service,
            groups: Arc::new(RwLock::new(HashMap::new())),
            consensus_service,
            eth_signer,
            handler,
            state_handler,
            default_group_config,
            scoring_service: Mutex::new(PeerScoringService::new(
                InMemoryPeerScoreStorage::new(),
                FixedScoringProvider::new(Self::default_score_deltas()),
                ScoringConfig {
                    default_score: 100,
                    removal_threshold: 0,
                },
            )),
        }
    }

    /// Default score deltas for all events.
    fn default_score_deltas() -> HashMap<ScoreEvent, i64> {
        HashMap::from([
            // ECP target penalties (violation-type-specific)
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::BrokenMlsProposal, -30),
            (ScoreEvent::CensorshipInactivity, -40),
            // ECP creator outcomes
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::EmergencyNoCreator, -50),
            // Commit selection (M2)
            (ScoreEvent::SuccessfulCommit, 10),
            // Commit validation (M5)
            (ScoreEvent::NonFinalizedProposalCommit, -30),
        ])
    }

    /// Lock the scoring service, recovering from a poisoned mutex.
    fn scoring(
        &self,
    ) -> std::sync::MutexGuard<'_, PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider>>
    {
        self.scoring_service
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Get the user's identity string (wallet address as checksummed hex).
    pub fn identity_string(&self) -> String {
        self.mls_service.wallet_hex()
    }

    // ─────────────────────────── Group Management ───────────────────────────

    /// Create or join a group with the user's default config.
    pub async fn create_group(
        &mut self,
        group_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        self.create_group_with_config(group_name, is_creation, self.default_group_config.clone())
            .await
    }

    /// Create or join a group with custom config.
    pub async fn create_group_with_config(
        &mut self,
        group_name: &str,
        is_creation: bool,
        config: GroupConfig,
    ) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        if groups.contains_key(group_name) {
            return Err(UserError::GroupAlreadyExists);
        }

        let (group, state_machine) = if is_creation {
            let group = core::create_group(group_name, &self.mls_service, config.protocol.clone())?;
            let state_machine = GroupStateMachine::new_as_member_with_config(config);
            (group, state_machine)
        } else {
            let group = core::prepare_to_join(
                group_name,
                self.mls_service.wallet_bytes(),
                config.protocol.clone(),
            );
            let state_machine = GroupStateMachine::new_as_pending_join_with_config(config);
            (group, state_machine)
        };

        let initial_state = state_machine.current_state();
        groups.insert(
            group_name.to_string(),
            GroupEntry {
                group,
                state_machine,
                app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            },
        );
        drop(groups);

        // Register creator in peer scoring (only if actually creating, not joining)
        if is_creation {
            self.scoring()
                .add_member(group_name, &self.mls_service.wallet_bytes());
        }

        self.state_handler
            .on_state_changed(group_name, initial_state)
            .await;

        Ok(())
    }

    /// Leave a group.
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!("[leave_group]: Leaving group {group_name}");

        let (old_state, new_state) = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            let old_state = entry.state_machine.current_state();
            match old_state {
                GroupState::PendingJoin => {
                    groups.remove(group_name);
                    drop(groups);
                    self.handler.on_leave_group(group_name).await?;
                    return Ok(());
                }
                GroupState::Reelection => {
                    return Err(UserError::GroupBlocked(old_state.to_string()));
                }
                GroupState::Leaving => return Err(UserError::AlreadyLeaving),
                _ => {
                    entry.state_machine.start_leaving();
                }
            }
            (old_state, entry.state_machine.current_state())
        };

        self.state_handler
            .on_state_changed(group_name, new_state.clone())
            .await;

        info!(
            "[leave_group]: Transitioning from {old_state} to Leaving, sending self-removal for group {group_name}"
        );

        self.start_voting_on_request_background(
            group_name.to_string(),
            GroupUpdateRequest {
                payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                    identity: parse_wallet_to_bytes(&self.identity_string())?,
                })),
            },
        )
        .await?;
        Ok(())
    }

    /// Get the state of a group.
    pub async fn get_group_state(&self, group_name: &str) -> Result<GroupState, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        Ok(entry.state_machine.current_state())
    }

    /// List all group names.
    pub async fn list_groups(&self) -> Vec<String> {
        let groups = self.groups.read().await;
        groups.keys().cloned().collect()
    }

    /// Get freeze round candidate count: (received, expected).
    ///
    /// `received` is the number of buffered commit candidates.
    /// `expected` is the steward list size (one candidate per steward).
    /// Returns `(0, 0)` if not in freeze or no steward list.
    pub async fn get_freeze_candidate_count(
        &self,
        group_name: &str,
    ) -> Result<(usize, usize), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let received = entry.group.freeze_candidate_count();
        let expected = entry.group.steward_list().map(|l| l.len()).unwrap_or(0);
        Ok((received, expected))
    }

    /// Check if the user is steward for a group.
    pub async fn is_steward_for_group(&self, group_name: &str) -> Result<bool, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        Ok(entry.group.is_steward())
    }

    /// Get the members of a group.
    pub async fn get_group_members(&self, group_name: &str) -> Result<Vec<String>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

        if !self.mls_service.has_group(entry.group.group_name()) {
            return Ok(Vec::new());
        }

        let members = core::group_members(&entry.group, &self.mls_service)?;
        Ok(members
            .into_iter()
            .map(|raw| format_wallet_address(raw.as_slice()).to_string())
            .collect())
    }

    /// Get member scores for a group from the peer scoring service.
    pub fn get_member_scores(&self, group_name: &str) -> Vec<(Vec<u8>, i64)> {
        self.scoring().all_members_with_scores(group_name)
    }

    /// Get the score for a specific member in a group.
    pub fn get_member_score(&self, group_name: &str, member_id: &[u8]) -> Option<i64> {
        self.scoring().score_for(group_name, member_id)
    }

    /// Get the steward role for each member in a group.
    ///
    /// Returns `(member_id_bytes, role)` pairs where role is one of:
    /// "epoch_steward", "backup_steward", "steward", or "member".
    pub async fn get_member_roles(
        &self,
        group_name: &str,
    ) -> Result<Vec<(Vec<u8>, String)>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let epoch = self.mls_service.current_epoch(group_name)?;
        let members = core::group_members(&entry.group, &self.mls_service)?;

        let list = entry.group.steward_list();
        let roles = members
            .into_iter()
            .map(|id| {
                let role = match list {
                    Some(l) if !l.is_exhausted(epoch) => {
                        if l.epoch_steward(epoch).is_some_and(|es| es == id) {
                            "epoch_steward"
                        } else if l.backup_steward(epoch).is_some_and(|bs| bs == id) {
                            "backup_steward"
                        } else if l.contains(&id) {
                            "steward"
                        } else {
                            "member"
                        }
                    }
                    Some(l) if l.contains(&id) => "steward",
                    _ => "member",
                };
                (id, role.to_string())
            })
            .collect();
        Ok(roles)
    }

    /// Sync the scoring service's member list with the MLS group's actual members.
    ///
    /// Adds any MLS members not yet tracked and removes scored members no longer in MLS.
    fn sync_scoring_members(&self, group_name: &str, group: &Group) {
        let mls_members = match core::group_members(group, &self.mls_service) {
            Ok(m) => m,
            Err(_) => return,
        };

        let mut scoring = self.scoring();
        let scored = scoring.all_members_with_scores(group_name);
        let scored_ids: std::collections::HashSet<Vec<u8>> =
            scored.iter().map(|(id, _)| id.clone()).collect();
        let mls_ids: std::collections::HashSet<Vec<u8>> = mls_members.iter().cloned().collect();

        for member_id in &mls_ids {
            if !scored_ids.contains(member_id) {
                scoring.add_member(group_name, member_id);
            }
        }
        for member_id in &scored_ids {
            if !mls_ids.contains(member_id) {
                scoring.remove_member(group_name, member_id);
            }
        }
    }

    /// Try to auto-fill the steward list when member count is below `sn_min`.
    ///
    /// After any commit that changes membership, the group may have fewer members
    /// than `sn_min`. Per RFC, when `member_count < sn_min` all members become
    /// stewards deterministically. This helper checks the condition and regenerates
    /// the steward list if needed.
    async fn try_auto_fill_steward_list(&self, group_name: &str) -> Result<(), UserError> {
        // Phase 1: read lock — gather member list and check sn_min threshold.
        let (needs_fill, members) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let members = core::group_members(&entry.group, &self.mls_service)?;
            let needs = members.len() < entry.group.protocol_config().sn_min;
            (needs, members)
        };

        if !needs_fill {
            return Ok(());
        }

        // Phase 2: write lock — generate and set new list.
        let epoch = self.mls_service.current_epoch(group_name)?;
        {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            let sn = entry
                .group
                .protocol_config()
                .compute_list_size(members.len());
            entry
                .group
                .generate_and_set_steward_list(epoch, &members, sn)?;
        }

        Ok(())
    }

    /// Check if the steward list is exhausted and initiate an election if needed.
    ///
    /// Called after epoch advance. If the list is exhausted for the current epoch,
    /// generates a deterministic proposed steward list and submits an election
    /// proposal for consensus.
    async fn try_initiate_steward_election(&self, group_name: &str) -> Result<(), UserError> {
        let (members, election_epoch, config) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let epoch = self.mls_service.current_epoch(group_name)?;
            if !entry.group.is_steward_list_exhausted(epoch) {
                return Ok(());
            }
            let members = core::group_members(&entry.group, &self.mls_service)?;
            let config = entry.group.protocol_config().clone();
            (members, epoch, config)
        };

        // Generate the proposed list deterministically
        let sn = config.compute_list_size(members.len());
        let proposed_list = crate::core::StewardList::generate(
            election_epoch,
            group_name.as_bytes(),
            &members,
            sn,
            config,
        )?;

        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards: proposed_list.members().to_vec(),
                    election_epoch,
                },
            )),
        };

        info!(
            "Steward list exhausted at epoch {election_epoch} for group {group_name}, \
             initiating election with {} stewards",
            proposed_list.len()
        );

        self.start_voting_on_request_background(group_name.to_string(), request)
            .await?;

        Ok(())
    }

    /// Post-epoch-advance steward list housekeeping.
    ///
    /// Runs the post-epoch sequence after any commit that advances the epoch:
    /// 1. Auto-fill steward list if membership dropped below sn_min
    /// 2. Initiate steward election if the list is exhausted
    ///
    /// Steward flag is derived automatically from `steward_list.contains(self_identity)`.
    async fn steward_list_housekeeping(&self, group_name: &str) -> Result<(), UserError> {
        self.try_auto_fill_steward_list(group_name).await?;
        // Election initiation may legitimately fail (e.g., group state doesn't
        // allow new proposals yet). Log and continue — don't block the caller.
        if let Err(e) = self.try_initiate_steward_election(group_name).await {
            info!("[steward_list_housekeeping] Election initiation deferred: {e}");
        }
        Ok(())
    }

    /// Send the current steward list to the group as an encrypted app message.
    ///
    /// Called by the steward after committing an Add that brings new members into
    /// the group. The new joiner's handle has `steward_list: None` until this
    /// sync message is received and validated.
    ///
    /// Existing members who already have a list will ignore it (idempotent).
    async fn send_steward_list_sync(&self, group_name: &str) -> Result<(), UserError> {
        let (sync_msg, app_id) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

            let list = match entry.group.steward_list() {
                Some(l) => l,
                None => return Ok(()), // no list to sync
            };

            let sync = StewardListSync {
                steward_members: list.members().to_vec(),
                start_epoch: list.start_epoch(),
                sn_min: list.config().sn_min as u32,
                sn_max: list.config().sn_max as u32,
            };

            let app_msg: AppMessage = sync.into();
            (app_msg, entry.app_id.clone())
        };

        let packet = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            core::build_message(&entry.group, &self.mls_service, &sync_msg, &app_id)?
        };

        self.handler.on_outbound(group_name, packet).await?;

        info!("[send_steward_list_sync] Sent steward list sync for group {group_name}");

        Ok(())
    }

    /// Check if any members are below the removal threshold and initiate ECPs.
    ///
    /// Only the steward initiates score-based removals. Skips self and any
    /// member for which a removal ECP is already pending.
    async fn check_and_initiate_score_removals(&self, group_name: &str) -> Result<(), UserError> {
        let (is_steward, epoch, self_id) = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            (
                entry.group.is_steward(),
                self.mls_service.current_epoch(group_name)?,
                self.mls_service.wallet_bytes(),
            )
        };

        if !is_steward {
            return Ok(());
        }

        let targets: Vec<(Vec<u8>, i64)> = {
            let scoring = self.scoring();
            scoring
                .members_below_threshold(group_name)
                .into_iter()
                .filter(|id| *id != self_id) // skip self (deferred to M2)
                .map(|id| {
                    let score = scoring.score_for(group_name, &id).unwrap_or(0);
                    (id, score)
                })
                .collect()
        };

        for (target_id, current_score) in targets {
            // Skip if already pending
            {
                let groups = self.groups.read().await;
                if let Some(entry) = groups.get(group_name) {
                    if entry.group.has_pending_removal(&target_id) {
                        continue;
                    }
                }
            }

            let evidence =
                ViolationEvidence::score_below_threshold(target_id.clone(), epoch, current_score)
                    .with_creator(self_id.clone());
            let request = evidence.into_update_request()?;

            // Track before submitting to prevent duplicates
            {
                let mut groups = self.groups.write().await;
                if let Some(entry) = groups.get_mut(group_name) {
                    entry.group.observe_pending_removal(target_id.clone());
                }
            }

            info!(
                "Steward initiating SCORE_BELOW_THRESHOLD removal for member {:?} \
                 (score={current_score}) in group {group_name}",
                target_id
            );
            if let Err(e) = self
                .start_voting_on_request_background(group_name.to_string(), request)
                .await
            {
                // Clean up pending target on failure so it can be retried next time.
                let mut groups = self.groups.write().await;
                if let Some(entry) = groups.get_mut(group_name) {
                    entry.group.resolve_pending_removal(&target_id);
                }
                error!(
                    "Failed to start SCORE_BELOW_THRESHOLD vote for {:?}: {e}",
                    target_id
                );
            }
        }

        Ok(())
    }

    /// Get current epoch proposals for a group.
    pub async fn get_approved_proposal_for_current_epoch(
        &self,
        group_name: &str,
    ) -> Result<Vec<GroupUpdateRequest>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let approved_proposals = entry.group.approved_proposals();
        let display_proposals: Vec<GroupUpdateRequest> = approved_proposals.into_values().collect();
        Ok(display_proposals)
    }

    /// Get epoch history for a group.
    pub async fn get_epoch_history(
        &self,
        group_name: &str,
    ) -> Result<Vec<Vec<GroupUpdateRequest>>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let history = entry.group.epoch_history();
        Ok(history
            .iter()
            .map(|batch| batch.values().cloned().collect())
            .collect())
    }

    // ─────────────────────────── Messaging ───────────────────────────

    /// Build and send a key package message for a group via the handler.
    pub async fn send_kp_message(&self, group_name: &str) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let packet =
            core::build_key_package_message(&entry.group, &self.mls_service, &entry.app_id)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Send a conversation message to a group.
    pub async fn send_app_message(
        &self,
        group_name: &str,
        message: Vec<u8>,
    ) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

        let state = entry.state_machine.current_state();
        if state == GroupState::PendingJoin {
            return Err(UserError::GroupBlocked(state.to_string()));
        }

        let app_msg: AppMessage = ConversationMessage {
            message,
            sender: self.identity_string(),
            group_name: group_name.to_string(),
        }
        .into();

        let packet = core::build_message(&entry.group, &self.mls_service, &app_msg, &entry.app_id)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Process a ban request.
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
    ) -> Result<(), UserError> {
        {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let state = entry.state_machine.current_state();
            if state != GroupState::Working {
                return Err(UserError::GroupBlocked(state.to_string()));
            }
        }

        self.start_voting_on_request_background(
            group_name.to_string(),
            GroupUpdateRequest {
                payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                    identity: parse_wallet_to_bytes(ban_request.user_to_ban.as_str())?,
                })),
            },
        )
        .await?;

        Ok(())
    }

    // ─────────────────────────── Steward Operations ───────────────────────────

    /// Check if still in pending join state.
    pub async fn check_pending_join(&self, group_name: &str) -> bool {
        let (state, expired) = {
            let groups = self.groups.read().await;
            match groups.get(group_name) {
                Some(entry) => (
                    entry.state_machine.current_state(),
                    entry.state_machine.is_pending_join_expired(),
                ),
                None => return false,
            }
        };

        if state != GroupState::PendingJoin {
            return false;
        }

        if expired {
            info!(
                "[check_pending_join]: Join timed out for group {group_name} \
                 (time-based fallback)"
            );
            self.groups.write().await.remove(group_name);
            let _ = self.handler.on_leave_group(group_name).await;
            return false;
        }

        true
    }

    /// Check if the freeze phase timed out.
    ///
    /// Call this periodically while a group is in `Freezing` state.
    pub async fn poll_freeze_status(&self, group_name: &str) -> FreezeTimeoutStatus {
        let has_proposals = {
            let mut groups = self.groups.write().await;
            let entry = match groups.get_mut(group_name) {
                Some(e) => e,
                None => return FreezeTimeoutStatus::NotFreezing,
            };

            let state = entry.state_machine.current_state();
            if state != GroupState::Freezing {
                return FreezeTimeoutStatus::NotFreezing;
            }
            if !entry.state_machine.is_freeze_timed_out() {
                return FreezeTimeoutStatus::StillFreezing;
            }

            entry.state_machine.start_selection();
            entry.group.approved_proposals_count() > 0
        };

        self.state_handler
            .on_state_changed(group_name, GroupState::Selection)
            .await;

        let finalize_result = {
            let mut groups = self.groups.write().await;
            let entry = match groups.get_mut(group_name) {
                Some(e) => e,
                None => return FreezeTimeoutStatus::NotFreezing,
            };
            let allow_subset = entry.group.allow_subset_candidates();
            match core::finalize_freeze_round(
                &mut entry.group,
                &self.mls_service,
                allow_subset,
                &entry.app_id,
            ) {
                Ok(result) => result,
                Err(e) => {
                    error!("[poll_freeze_status] finalize_freeze_round failed: {e}");
                    FreezeFinalizeResult::NoCandidate
                }
            }
        };

        match finalize_result {
            FreezeFinalizeResult::Applied { result, outbound } => {
                // Send deferred welcome packets now that commit is merged
                let has_welcome = outbound
                    .iter()
                    .any(|p| p.subtopic == crate::ds::WELCOME_SUBTOPIC);
                for packet in outbound {
                    if let Err(e) = self.handler.on_outbound(group_name, packet).await {
                        error!("[poll_freeze_status] Failed to send deferred welcome: {e}");
                    }
                }

                // Send steward list sync to new joiners after welcome packets
                if has_welcome {
                    if let Err(e) = self.send_steward_list_sync(group_name).await {
                        error!("[poll_freeze_status] Failed to send steward list sync: {e}");
                    }
                }

                // Dispatch result first — this transitions state to Working
                if let Err(e) = self.handle_process_result(group_name, result).await {
                    error!("[poll_freeze_status] Failed to dispatch finalize result: {e}");
                }
                return FreezeTimeoutStatus::Applied;
            }
            FreezeFinalizeResult::NoCandidate => {
                let (next_state, should_accuse, violation_epoch, steward_id) = {
                    let mut groups = self.groups.write().await;
                    let entry = match groups.get_mut(group_name) {
                        Some(e) => e,
                        None => return FreezeTimeoutStatus::TimedOut { has_proposals },
                    };

                    if has_proposals {
                        entry.group.reject_all_approved_proposals();
                        entry.group.reject_all_voting_proposals();
                        entry.state_machine.clear_proposal_timer();
                        entry.state_machine.start_reelection();
                        let violation_epoch =
                            self.mls_service.current_epoch(group_name).unwrap_or(0);
                        let steward_id = entry
                            .group
                            .epoch_steward(violation_epoch)
                            .filter(|id| !id.is_empty())
                            .map(|id| id.to_vec())
                            .unwrap_or_default();
                        (
                            GroupState::Reelection,
                            !steward_id.is_empty(),
                            violation_epoch,
                            steward_id,
                        )
                    } else {
                        entry.group.clear_freeze_round();

                        entry.state_machine.start_working();
                        (GroupState::Working, false, 0, Vec::new())
                    }
                };

                self.state_handler
                    .on_state_changed(group_name, next_state.clone())
                    .await;

                if should_accuse {
                    let request =
                        match ViolationEvidence::censorship_inactivity(steward_id, violation_epoch)
                            .with_creator(self.mls_service.wallet_bytes())
                            .into_update_request()
                        {
                            Ok(r) => r,
                            Err(e) => {
                                error!("[poll_freeze_status] Failed to build ECP: {e}");
                                return FreezeTimeoutStatus::TimedOut { has_proposals };
                            }
                        };
                    if let Err(e) = self
                        .start_voting_on_request_background(group_name.to_string(), request)
                        .await
                    {
                        error!("[poll_freeze_status] Failed to start emergency criteria vote: {e}");
                    }
                }
            }
        }

        FreezeTimeoutStatus::TimedOut { has_proposals }
    }

    /// Check for steward inactivity and transition to Freezing if needed.
    ///
    /// If approved proposals exist and the epoch steward hasn't committed
    /// within `epoch_duration`, transition to Freezing so the freeze timeout
    /// can detect steward fault. Any member (steward or not) can detect this.
    pub async fn check_member_freeze(&self, group_name: &str) -> Result<bool, UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        let state = entry.state_machine.current_state();
        if state == GroupState::PendingJoin || state == GroupState::Leaving {
            return Ok(false);
        }

        let proposal_count = entry.group.approved_proposals_count();
        let entered_freezing = entry.state_machine.check_steward_inactivity(proposal_count);
        if entered_freezing {
            let epoch = self.mls_service.current_epoch(group_name)?;
            entry.group.ensure_freeze_round(epoch);
            let is_steward = entry.group.is_steward();

            let new_state = entry.state_machine.current_state();
            info!(
                "[check_member_freeze]: Steward inactivity → {} for group {group_name} \
                 ({proposal_count} approved proposals)",
                new_state
            );

            // If this member is a steward, create a commit candidate immediately.
            if is_steward {
                let messages =
                    create_commit_candidate(&mut entry.group, &self.mls_service, &entry.app_id)?;
                drop(groups);
                self.state_handler
                    .on_state_changed(group_name, new_state.clone())
                    .await;
                for message in messages {
                    self.handler.on_outbound(group_name, message).await?;
                }
            } else {
                drop(groups);
                self.state_handler
                    .on_state_changed(group_name, new_state.clone())
                    .await;
            }
        }

        Ok(entered_freezing)
    }

    pub async fn start_voting_on_request_background(
        &self,
        group_name: String,
        upd_request: GroupUpdateRequest,
    ) -> Result<(), UserError> {
        let is_emergency = matches!(
            upd_request.payload,
            Some(group_update_request::Payload::EmergencyCriteria(_))
        );
        let expected_voters = {
            let groups = self.groups.read().await;
            let entry = groups.get(&group_name).ok_or(UserError::GroupNotFound)?;
            let state = entry.state_machine.current_state();
            match state {
                GroupState::Reelection => {
                    if !is_emergency {
                        return Err(UserError::GroupBlocked(state.to_string()));
                    }
                }
                GroupState::Freezing | GroupState::Selection => {
                    return Err(UserError::GroupBlocked(state.to_string()));
                }
                _ => {
                    // RFC §"Partial Freeze Semantics": while any emergency criteria proposal
                    // is unresolved, lower-priority proposals MUST NOT be created.
                    // This blocks both election and commit proposals during active emergency.
                    if !is_emergency && entry.group.has_active_emergency() {
                        return Err(UserError::PartialFreeze);
                    }
                }
            }
            let members = core::group_members(&entry.group, &self.mls_service)?;
            members.len() as u32
        };
        let identity_string = self.mls_service.wallet_hex();

        let consensus = Arc::clone(&self.consensus_service);
        let groups = Arc::clone(&self.groups);
        let handler = Arc::clone(&self.handler);
        let group_name_clone = group_name.clone();

        tokio::spawn(async move {
            let result: Result<(), UserError> = async {
                // Step 1: submit to consensus (network call, no lock held)
                let (proposal_id, vote_payload) = start_voting::<P>(
                    &group_name,
                    &upd_request,
                    expected_voters,
                    identity_string,
                    &*consensus,
                )
                .await?;

                // Step 2: store ownership and register active emergency (if applicable)
                // before emitting UI notification. Storing first ensures
                // `handle_consensus_event` will see `is_owner=true` even if a
                // consensus result arrives immediately.
                {
                    let mut groups = groups.write().await;
                    if let Some(entry) = groups.get_mut(&group_name) {
                        entry
                            .group
                            .store_voting_proposal(proposal_id, upd_request);
                        if is_emergency {
                            entry.group.observe_emergency(proposal_id);
                        }
                    } else {
                        error!(
                            "[start_voting_on_request]: Group {group_name} missing during proposal store"
                        );
                    }
                }

                info!(
                    "[start_voting_on_request]: Stored voting proposal: {proposal_id}"
                );

                // Step 3: notify UI (after ownership is recorded)
                handler.on_app_message(&group_name, vote_payload).await?;

                Ok(())
            }
            .await;

            if let Err(err) = result {
                error!("[start_voting_on_request]: background task failed: {err}");
                handler
                    .on_error(&group_name_clone, "Start voting", &err.to_string())
                    .await;
            }
        });

        Ok(())
    }

    // ─────────────────────────── Voting ───────────────────────────

    /// Process a user vote.
    pub async fn process_user_vote(
        &mut self,
        group_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

        let state = entry.state_machine.current_state();
        if state == GroupState::Freezing || state == GroupState::Selection {
            return Err(UserError::GroupBlocked(state.to_string()));
        }

        let group = entry.group.clone();
        let app_id = entry.app_id.clone();
        drop(groups);

        cast_vote::<P, _, _>(
            &group,
            proposal_id,
            vote,
            &*self.consensus_service,
            self.eth_signer.clone(),
            &self.mls_service,
            &*self.handler,
            &app_id,
        )
        .await?;
        Ok(())
    }

    // ─────────────────────────── Inbound Processing ───────────────────────────

    /// Dispatches a single ProcessResult to the appropriate handler/consensus/state-machine action.
    /// Used by process_inbound_packet() and freeze finalization.
    async fn handle_process_result(
        &self,
        group_name: &str,
        result: ProcessResult,
    ) -> Result<(), UserError> {
        match result {
            ProcessResult::AppMessage(msg) => {
                self.handler.on_app_message(group_name, msg).await?;
            }
            ProcessResult::Proposal(proposal) => {
                // RFC §"Partial Freeze Semantics": track any incoming emergency proposal
                // so the partial freeze applies to all peers, not just the creator.
                if let Ok(req) = GroupUpdateRequest::decode(proposal.payload.as_slice()) {
                    if matches!(
                        req.payload,
                        Some(group_update_request::Payload::EmergencyCriteria(_))
                    ) {
                        let mut groups = self.groups.write().await;
                        if let Some(entry) = groups.get_mut(group_name) {
                            entry.group.observe_emergency(proposal.proposal_id);
                        }
                    }
                }
                // TODO(M3c): RFC §"Partial Freeze Semantics" also requires that
                // lower-priority proposals received from peers are DROPPED when an
                // emergency is active — not just blocked locally. `forward_incoming_proposal`
                // feeds the local consensus service and cannot be filtered here without
                // risk of consensus inconsistency (other nodes may already have accepted
                // the proposal). Full enforcement requires consensus-service-level
                // priority gating (see ROADMAP.md M3c §3c.1).
                forward_incoming_proposal::<P>(
                    group_name,
                    proposal,
                    &*self.consensus_service,
                    &*self.handler,
                )
                .await?;
            }
            ProcessResult::Vote(vote) => {
                forward_incoming_vote::<P>(group_name, vote, &*self.consensus_service).await?;
            }
            ProcessResult::MembershipChangeReceived(request) => {
                self.start_voting_on_request_background(group_name.to_string(), request)
                    .await?;
            }
            ProcessResult::JoinedGroup(name) => {
                // Build "User joined" system message (moved from core)
                let msg: AppMessage = ConversationMessage {
                    message: format!("User {} joined the group", self.mls_service.wallet_hex())
                        .into_bytes(),
                    sender: "SYSTEM".to_string(),
                    group_name: name.clone(),
                }
                .into();

                // Build and send outbound packet
                let packet = {
                    let groups = self.groups.read().await;
                    if let Some(entry) = groups.get(&name) {
                        core::build_message(&entry.group, &self.mls_service, &msg, &entry.app_id)?
                    } else {
                        return Ok(());
                    }
                };
                self.handler.on_outbound(&name, packet).await?;
                self.handler.on_joined_group(&name).await?;

                // Sync MLS members into peer scoring
                {
                    let groups = self.groups.read().await;
                    if let Some(entry) = groups.get(&name) {
                        self.sync_scoring_members(&name, &entry.group);
                    }
                }

                // Sync epoch boundary and transition to Working
                let state = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(&name) {
                        entry.state_machine.start_working();
                        Some(entry.state_machine.current_state())
                    } else {
                        None
                    }
                };
                if let Some(state) = state {
                    self.state_handler.on_state_changed(&name, state).await;
                }
            }
            ProcessResult::GroupUpdated => {
                // TODO(M2): Reward the commit author with SuccessfulCommit once
                // ProcessResult/FreezeFinalizeResult carries the commit sender identity.
                // In multi-steward mode the winner may differ from the epoch steward.

                // Sync member list in peer scoring (commit may add/remove members)
                {
                    let groups = self.groups.read().await;
                    if let Some(entry) = groups.get(group_name) {
                        self.sync_scoring_members(group_name, &entry.group);
                    }
                }

                // Transition to Working BEFORE steward checks (election needs Working state)
                let transitioned = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(group_name) {
                        let state = entry.state_machine.current_state();
                        if state == GroupState::Working
                            || state == GroupState::Freezing
                            || state == GroupState::Selection
                            || state == GroupState::Reelection
                        {
                            entry.state_machine.start_working();
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                // Now run steward checks (auto-fill, flag sync, election) in Working state
                self.steward_list_housekeeping(group_name).await?;

                if transitioned {
                    self.state_handler
                        .on_state_changed(group_name, GroupState::Working)
                        .await;
                }
            }
            ProcessResult::LeaveGroup => {
                self.groups.write().await.remove(group_name);
                self.handler.on_leave_group(group_name).await?;
            }
            ProcessResult::ViolationDetected(evidence) => {
                info!(
                    "Violation detected: type={}, target={:?}",
                    evidence.violation_type, evidence.target_member_id
                );
                let evidence = evidence.with_creator(self.mls_service.wallet_bytes());
                self.start_voting_on_request_background(
                    group_name.to_string(),
                    evidence.into_update_request()?,
                )
                .await?;
            }
            ProcessResult::CommitCandidateReceived => {
                let transitioned = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(group_name) {
                        // Enter Freezing on candidate receipt if in Working state.
                        // The state guard prevents double-entry if already freezing.
                        if entry.state_machine.current_state() == GroupState::Working {
                            entry.state_machine.start_freezing();
                            let epoch = self.mls_service.current_epoch(group_name).unwrap_or(0);
                            entry.group.ensure_freeze_round(epoch);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if transitioned {
                    self.state_handler
                        .on_state_changed(group_name, GroupState::Freezing)
                        .await;
                }
            }
            ProcessResult::StewardListSyncReceived(sync) => {
                // Phase 1: read lock — check if list needed, fetch members.
                let members = {
                    let groups = self.groups.read().await;
                    let Some(entry) = groups.get(group_name) else {
                        return Ok(());
                    };
                    if entry.group.steward_list().is_some() {
                        None // Already has list, skip
                    } else {
                        Some(core::group_members(&entry.group, &self.mls_service)?)
                    }
                };

                let Some(members) = members else {
                    return Ok(());
                };

                // Validate: (1) all proposed stewards are current group members,
                // (2) the list ordering is self-consistent (matches deterministic
                // generation with the steward members as the candidate pool).
                // We validate against steward_members, NOT full group, because
                // the list may have been generated before the joiner existed.
                let all_present = StewardList::validate_members(&sync.steward_members, &members);
                let ordering_valid = StewardList::validate(
                    &sync.steward_members,
                    sync.start_epoch,
                    group_name.as_bytes(),
                    &sync.steward_members,
                    &ProtocolConfig::new(sync.sn_min as usize, sync.sn_max as usize)
                        .unwrap_or_default(),
                )?;

                if all_present && ordering_valid {
                    // Phase 2: write lock — apply validated list.
                    let sn = sync.steward_members.len();
                    {
                        let mut groups = self.groups.write().await;
                        if let Some(entry) = groups.get_mut(group_name) {
                            entry.group.generate_and_set_steward_list(
                                sync.start_epoch,
                                &sync.steward_members,
                                sn,
                            )?;
                        }
                    }
                    info!(
                        "[StewardListSyncReceived] Applied steward list for group \
                         {group_name} (start_epoch={}, len={sn})",
                        sync.start_epoch,
                    );
                } else {
                    info!(
                        "[StewardListSyncReceived] Ignoring invalid sync for group \
                         {group_name} (present={all_present}, ordering={ordering_valid})"
                    );
                }
            }
            ProcessResult::Noop => {}
        }
        Ok(())
    }

    /// Process an inbound packet.
    pub async fn process_inbound_packet(&self, packet: InboundPacket) -> Result<(), UserError> {
        let group_name = packet.group_id.clone();

        // Check if message is from same app instance
        {
            let groups = self.groups.read().await;
            if let Some(entry) = groups.get(&group_name) {
                if packet.app_id == entry.app_id {
                    return Ok(());
                }
            } else {
                return Err(UserError::GroupNotFound);
            }
        }

        // Process the packet
        let result = {
            let mut groups = self.groups.write().await;
            let entry = groups
                .get_mut(&group_name)
                .ok_or(UserError::GroupNotFound)?;

            core::process_inbound(
                &mut entry.group,
                &packet.payload,
                &packet.subtopic,
                &self.mls_service,
            )?
        };

        self.handle_process_result(&group_name, result).await
    }

    // ─────────────────────────── Consensus Events ───────────────────────────

    /// Handle a consensus event.
    pub async fn handle_consensus_event(
        &mut self,
        group_name: &str,
        event: ConsensusEvent,
    ) -> Result<(), UserError> {
        let (proposal_id, approved) = match &event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
                ..
            } => (*proposal_id, *result),
            ConsensusEvent::ConsensusFailed { proposal_id, .. } => (*proposal_id, false),
        };

        // Fetch payload from consensus service (no lock held).
        let scope = P::Scope::from(group_name.to_string());
        let payload = self
            .consensus_service
            .get_proposal_payload(&scope, proposal_id)
            .await?;

        // Apply consensus result (write lock)
        let consensus_apply = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

            let approved_before = entry.group.approved_proposals_count();

            info!("Consensus reached for proposal {proposal_id}: approved={approved}");
            let result =
                core::apply_consensus_result(&mut entry.group, proposal_id, approved, &payload)?;

            let approved_after = entry.group.approved_proposals_count();
            entry
                .state_machine
                .notify_proposal_approved(approved_before, approved_after);

            result
        };

        // ── Handle election outcome ──
        if let Some(election) = consensus_apply.election {
            // Validate the proposed list against the current member set.
            let (is_valid, _) = {
                let groups = self.groups.read().await;
                let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
                let members = core::group_members(&entry.group, &self.mls_service)?;
                let valid = core::validate_election_proposal(
                    &entry.group,
                    &election.proposed_stewards,
                    election.election_epoch,
                    &members,
                )?;
                (valid, members)
            };

            if is_valid {
                {
                    let mut groups = self.groups.write().await;
                    let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
                    core::apply_election_result(
                        &mut entry.group,
                        &election.proposed_stewards,
                        election.election_epoch,
                    )?;
                }
                // Sync steward flag from the new list.
                info!(
                    "Steward election applied for group {group_name}: \
                     epoch={}, stewards={}",
                    election.election_epoch,
                    election.proposed_stewards.len()
                );
            } else {
                info!("Steward election proposal rejected (invalid list) for group {group_name}");
            }
            return Ok(());
        }

        // ── Handle emergency score ops ──
        if !consensus_apply.score_ops.is_empty() {
            // Emergency proposal was resolved — apply scores and lift partial freeze.
            {
                let mut scoring = self.scoring();
                for op in &consensus_apply.score_ops {
                    scoring.apply_event(group_name, &op.member_id, op.event);
                }
            }

            // Resolve removal target tracking for SCORE_BELOW_THRESHOLD ECPs.
            // Extract the target_member_id from the payload evidence.
            if let Ok(req) = GroupUpdateRequest::decode(payload.as_slice()) {
                if let Some(group_update_request::Payload::EmergencyCriteria(ec)) = &req.payload {
                    if let Some(ev) = &ec.evidence {
                        let mut groups = self.groups.write().await;
                        if let Some(entry) = groups.get_mut(group_name) {
                            entry.group.resolve_pending_removal(&ev.target_member_id);
                        }
                    }
                }
            }

            // RFC §"Partial Freeze Semantics": lift the freeze once the emergency
            // proposal is resolved (approved or rejected — either way it's finalized).
            {
                let mut groups = self.groups.write().await;
                if let Some(entry) = groups.get_mut(group_name) {
                    entry.group.resolve_emergency(proposal_id);
                }
            }

            // After scoring changes, check if any members now fall below the threshold.
            if let Err(e) = self.check_and_initiate_score_removals(group_name).await {
                error!("[handle_consensus_event] check_and_initiate_score_removals failed: {e}");
            }
        }

        Ok(())
    }
}

// ─────────────────────────── DefaultProvider Convenience ───────────────────────────

impl<H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<DefaultProvider, H, SCH>
{
    /// Convenience constructor for the default provider with default group config.
    pub fn with_private_key(
        private_key: &str,
        consensus_service: Arc<DefaultConsensusService>,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
    ) -> Result<Self, UserError> {
        Self::with_private_key_and_config(
            private_key,
            consensus_service,
            handler,
            state_handler,
            GroupConfig::default(),
        )
    }

    /// Convenience constructor for the default provider with custom group config.
    pub fn with_private_key_and_config(
        private_key: &str,
        consensus_service: Arc<DefaultConsensusService>,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
        default_group_config: GroupConfig,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = signer.address();

        let mls_service = MlsService::new(MemoryDeMlsStorage::new());
        mls_service
            .init(user_address)
            .map_err(|e| UserError::Core(e.into()))?;

        Ok(Self::new_with_config(
            mls_service,
            consensus_service,
            signer,
            handler,
            state_handler,
            default_group_config,
        ))
    }
}
