//! User struct for managing multiple groups.
//!
//! This is the main entry point for the application layer,
//! managing multiple `GroupHandle`s and coordinating operations.

use alloy::signers::local::{LocalSignerError, PrivateKeySigner};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::sync::RwLock;
use tracing::{error, info};

use hashgraph_like_consensus::{
    api::ConsensusServiceAPI, service::DefaultConsensusService, types::ConsensusEvent,
};

use crate::app::state_machine::{
    CommitTimeoutStatus, GroupConfig, GroupState, GroupStateMachine, StateChangeHandler,
};
use crate::core::{
    self, ConsensusOutcome, CoreError, DeMlsProvider, DefaultProvider, GroupEventHandler,
    GroupHandle, ProcessResult, create_batch_proposals,
};
use crate::ds::InboundPacket;
use crate::mls_crypto::{
    IdentityError, MemoryDeMlsStorage, MlsService, format_wallet_address, parse_wallet_to_bytes,
};
use crate::protos::de_mls::messages::v1::{
    AppMessage, BanRequest, ConversationMessage, GroupUpdateRequest, RemoveMember,
    ViolationEvidence, group_update_request,
};

/// Internal state for a group managed by User.
struct GroupEntry {
    handle: GroupHandle,
    state_machine: GroupStateMachine,
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
        }
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

        let (handle, state_machine) = if is_creation {
            let handle = core::create_group_with_policy(
                group_name,
                &self.mls_service,
                config.quarantine_policy.clone(),
            )?;
            let state_machine = GroupStateMachine::new_as_steward_with_config(config);
            (handle, state_machine)
        } else {
            let handle =
                core::prepare_to_join_with_policy(group_name, config.quarantine_policy.clone());
            let state_machine = GroupStateMachine::new_as_pending_join_with_config(config);
            (handle, state_machine)
        };

        let initial_state = state_machine.current_state();
        groups.insert(
            group_name.to_string(),
            GroupEntry {
                handle,
                state_machine,
            },
        );
        drop(groups);

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

    /// Check if the user is steward for a group.
    pub async fn is_steward_for_group(&self, group_name: &str) -> Result<bool, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        Ok(entry.state_machine.is_steward())
    }

    /// Get the members of a group.
    pub async fn get_group_members(&self, group_name: &str) -> Result<Vec<String>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

        if !entry.handle.is_mls_initialized() {
            return Ok(Vec::new());
        }

        let members = core::group_members(&entry.handle, &self.mls_service)?;
        Ok(members
            .into_iter()
            .map(|raw| format_wallet_address(raw.as_slice()).to_string())
            .collect())
    }

    /// Get current epoch proposals for a group.
    pub async fn get_approved_proposal_for_current_epoch(
        &self,
        group_name: &str,
    ) -> Result<Vec<GroupUpdateRequest>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let approved_proposals = core::approved_proposals(&entry.handle);
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
        let history = core::epoch_history(&entry.handle);
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
        let packet = core::build_key_package_message(&entry.handle, &self.mls_service)?;
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
        if state == GroupState::PendingJoin || state == GroupState::Waiting {
            return Err(UserError::GroupBlocked(state.to_string()));
        }

        let app_msg: AppMessage = ConversationMessage {
            message,
            sender: self.identity_string(),
            group_name: group_name.to_string(),
        }
        .into();

        let packet = core::build_message(&entry.handle, &self.mls_service, &app_msg)?;
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

    /// Get the time until the next epoch boundary for a group.
    pub async fn time_until_next_epoch(&self, group_name: &str) -> Option<std::time::Duration> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name)?;
        entry.state_machine.time_until_next_boundary()
    }

    /// Check if the commit has timed out while in Waiting state.
    pub async fn check_commit_timeout(&self, group_name: &str) -> CommitTimeoutStatus {
        let (has_proposals, should_accuse, violation_epoch, steward_id) = {
            let mut groups = self.groups.write().await;
            let entry = match groups.get_mut(group_name) {
                Some(e) => e,
                None => return CommitTimeoutStatus::NotWaiting,
            };

            if entry.state_machine.current_state() != GroupState::Waiting {
                return CommitTimeoutStatus::NotWaiting;
            }
            if !entry.state_machine.is_commit_timed_out() {
                return CommitTimeoutStatus::StillWaiting;
            }

            let has_proposals = core::approved_proposals_count(&entry.handle) > 0;
            let epoch = entry.handle.current_epoch();
            let steward_id = entry
                .handle
                .steward_identity()
                .filter(|id| !id.is_empty())
                .map(|id| id.to_vec());

            if has_proposals {
                entry.handle.clear_approved_proposals();
            }

            let should_accuse = has_proposals && steward_id.is_some();

            entry.state_machine.sync_epoch_boundary();
            entry.state_machine.start_working();
            (
                has_proposals,
                should_accuse,
                epoch,
                steward_id.unwrap_or_default(),
            )
        };

        self.state_handler
            .on_state_changed(group_name, GroupState::Working)
            .await;

        if should_accuse {
            let request = ViolationEvidence::censorship_inactivity(steward_id, violation_epoch)
                .into_update_request();
            if let Err(e) = self
                .start_voting_on_request_background(group_name.to_string(), request)
                .await
            {
                error!("[check_commit_timeout] Failed to start emergency criteria vote: {e}");
            }
        }

        CommitTimeoutStatus::TimedOut { has_proposals }
    }

    /// Start a member epoch check.
    pub async fn start_member_epoch(&self, group_name: &str) -> Result<bool, UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        if entry.state_machine.is_steward() {
            return Ok(false);
        }

        let state = entry.state_machine.current_state();
        if state == GroupState::PendingJoin || state == GroupState::Leaving {
            return Ok(false);
        }

        let proposal_count = core::approved_proposals_count(&entry.handle);
        let entered_waiting = entry.state_machine.check_epoch_boundary(proposal_count);

        if entered_waiting {
            let new_state = entry.state_machine.current_state();
            info!(
                "[start_member_epoch]: Entered Waiting at epoch boundary for group {group_name} \
                 ({proposal_count} approved proposals)"
            );
            drop(groups);
            self.state_handler
                .on_state_changed(group_name, new_state.clone())
                .await;
        }

        Ok(entered_waiting)
    }

    /// Start a steward epoch.
    pub async fn start_steward_epoch(&mut self, group_name: &str) -> Result<(), UserError> {
        let has_proposals = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            core::approved_proposals_count(&entry.handle) > 0
        };

        if !has_proposals {
            return Ok(());
        }

        {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            entry.state_machine.start_steward_epoch()?;
        }
        self.state_handler
            .on_state_changed(group_name, GroupState::Waiting)
            .await;

        let messages = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            create_batch_proposals(&mut entry.handle, &self.mls_service)?
        };

        for message in messages {
            self.handler.on_outbound(group_name, message).await?;
        }

        {
            let mut groups = self.groups.write().await;
            if let Some(entry) = groups.get_mut(group_name) {
                entry.state_machine.start_working();
            }
        }
        self.state_handler
            .on_state_changed(group_name, GroupState::Working)
            .await;

        Ok(())
    }

    pub async fn start_voting_on_request_background(
        &self,
        group_name: String,
        upd_request: GroupUpdateRequest,
    ) -> Result<(), UserError> {
        let expected_voters = {
            let groups = self.groups.read().await;
            let entry = groups.get(&group_name).ok_or(UserError::GroupNotFound)?;
            let members = core::group_members(&entry.handle, &self.mls_service)?;
            members.len() as u32
        };
        let identity_string = self.mls_service.wallet_hex();

        let consensus = Arc::clone(&self.consensus_service);
        let groups = Arc::clone(&self.groups);
        let handler = Arc::clone(&self.handler);
        let group_name_clone = group_name.clone();

        tokio::spawn(async move {
            let result: Result<(), CoreError> = async {
                let proposal_id = core::start_voting::<P>(
                    &group_name,
                    &upd_request,
                    expected_voters,
                    identity_string,
                    &*consensus,
                    &*handler,
                )
                .await?;

                {
                    let mut groups = groups.write().await;
                    if let Some(entry) = groups.get_mut(&group_name) {
                        entry
                            .handle
                            .store_voting_proposal(proposal_id, upd_request);
                    } else {
                        error!(
                            "[start_voting_on_request]: Group {group_name} missing during proposal store"
                        );
                    }
                }

                info!(
                    "[start_voting_on_request]: Stored voting proposal: {proposal_id}"
                );

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
        if state == GroupState::Waiting {
            return Err(UserError::GroupBlocked(state.to_string()));
        }

        let handle = entry.handle.clone();
        drop(groups);

        core::cast_vote::<P, _, _>(
            &handle,
            group_name,
            proposal_id,
            vote,
            &*self.consensus_service,
            self.eth_signer.clone(),
            &self.mls_service,
            &*self.handler,
        )
        .await?;
        Ok(())
    }

    // ─────────────────────────── Inbound Processing ───────────────────────────

    /// Dispatches a single ProcessResult to the appropriate handler/consensus/state-machine action.
    /// Used by both process_inbound_packet() and retry_quarantined() call sites.
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
                core::forward_incoming_proposal::<P>(
                    group_name,
                    proposal,
                    &*self.consensus_service,
                    &*self.handler,
                )
                .await?;
            }
            ProcessResult::Vote(vote) => {
                core::forward_incoming_vote::<P>(group_name, vote, &*self.consensus_service)
                    .await?;
            }
            ProcessResult::GetUpdateRequest(request) => {
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
                        core::build_message(&entry.handle, &self.mls_service, &msg)?
                    } else {
                        return Ok(());
                    }
                };
                self.handler.on_outbound(&name, packet).await?;
                self.handler.on_joined_group(&name).await?;

                // Sync epoch boundary and transition to Working
                let state = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(&name) {
                        entry.state_machine.sync_epoch_boundary();
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
                let transitioned = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(group_name) {
                        let state = entry.state_machine.current_state();
                        if state == GroupState::Working || state == GroupState::Waiting {
                            entry.state_machine.sync_epoch_boundary();
                            entry.state_machine.start_working();
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
                self.start_voting_on_request_background(
                    group_name.to_string(),
                    evidence.into_update_request(),
                )
                .await?;
            }
            ProcessResult::BatchQuarantined {
                batch_proposal_ids,
                epoch,
                ..
            } => {
                tracing::debug!(
                    "Batch quarantined for group {}: proposal_ids={:?}, epoch={}",
                    group_name,
                    batch_proposal_ids,
                    epoch
                );
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
                if packet.app_id == entry.handle.app_id() {
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
                &mut entry.handle,
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
        // Phase 1: Check ownership (read lock)
        let (proposal_id, approved, is_owner) = match &event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
                ..
            } => {
                let groups = self.groups.read().await;
                let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
                let is_owner = entry.handle.is_owner_of_proposal(*proposal_id);
                (*proposal_id, *result, is_owner)
            }
            ConsensusEvent::ConsensusFailed { proposal_id, .. } => {
                let groups = self.groups.read().await;
                let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
                let is_owner = entry.handle.is_owner_of_proposal(*proposal_id);
                (*proposal_id, false, is_owner)
            }
        };

        // Phase 2: Fetch payload if needed (NO lock held)
        let payload = if approved && !is_owner {
            let scope = P::Scope::from(group_name.to_string());
            Some(
                self.consensus_service
                    .get_proposal_payload(&scope, proposal_id)
                    .await?,
            )
        } else {
            None
        };

        // Phase 3: Build typed outcome + apply mutation (write lock, sync)
        let outcome_result = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

            match (approved, is_owner) {
                (true, true) => {
                    info!("Consensus reached for proposal {proposal_id}: result=true (owner)");
                    core::apply_consensus_result(
                        &mut entry.handle,
                        proposal_id,
                        ConsensusOutcome::ApprovedOwner,
                    )
                }
                (true, false) => {
                    info!("Consensus reached for proposal {proposal_id}: result=true (non-owner)");
                    core::apply_consensus_result(
                        &mut entry.handle,
                        proposal_id,
                        ConsensusOutcome::Approved {
                            payload: payload.as_deref().unwrap(),
                        },
                    )
                }
                (false, _) => {
                    info!("Consensus reached for proposal {proposal_id}: result=false");
                    core::apply_consensus_result(
                        &mut entry.handle,
                        proposal_id,
                        ConsensusOutcome::Rejected,
                    )
                }
            }
        };
        outcome_result?;

        // Phase 4: Retry quarantined batches
        let quarantine_results = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            core::retry_quarantined(&mut entry.handle, &self.mls_service)?
        };

        for result in quarantine_results {
            self.handle_process_result(group_name, result).await?;
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

// ─────────────────────────── Errors ───────────────────────────

/// Errors from User operations.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("Group already exists")]
    GroupAlreadyExists,

    #[error("Group not found")]
    GroupNotFound,

    #[error("Already leaving this group")]
    AlreadyLeaving,

    #[error("Cannot send message: group is in {0} state")]
    GroupBlocked(String),

    #[error("Core error: {0}")]
    Core(#[from] CoreError),

    #[error("State machine error: {0}")]
    StateMachine(#[from] super::state_machine::StateMachineError),

    #[error("Consensus error: {0}")]
    Consensus(#[from] hashgraph_like_consensus::error::ConsensusError),

    #[error("Message error: {0}")]
    Message(#[from] prost::DecodeError),

    #[error("System time error: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),

    #[error("Signer error: {0}")]
    Signer(#[from] LocalSignerError),

    #[error("Identity error: {0}")]
    Identity(#[from] IdentityError),
}

impl UserError {
    pub fn is_fatal(&self) -> bool {
        matches!(self, UserError::GroupNotFound | UserError::AlreadyLeaving)
    }
}
