//! User struct for managing multiple groups.
//!
//! This is the main entry point for the application layer,
//! managing multiple `GroupHandle`s and coordinating operations.

use alloy::signers::local::{LocalSignerError, PrivateKeySigner};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::sync::RwLock;
use tracing::{error, info};

use hashgraph_like_consensus::{service::DefaultConsensusService, types::ConsensusEvent};

use crate::app::state_machine::{
    CommitTimeoutStatus, GroupConfig, GroupState, GroupStateMachine, StateChangeHandler,
};
use crate::core::{
    self, CoreError, DeMlsProvider, DefaultProvider, GroupEventHandler, GroupHandle,
    create_batch_proposals,
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
///
/// This struct provides the main application-level interface for
/// working with MLS groups, handling consensus, and processing messages.
///
/// The type parameter `P` determines which service implementations are used
/// (storage, consensus). Use [`DefaultProvider`] for standard configuration.
///
/// The type parameter `H` is the handler that receives output events
/// (outbound packets, app messages, leave/join notifications).
///
/// The type parameter `SCH` is the handler for state machine state changes
/// (an app-layer concern, separate from the core `GroupEventHandler`).
pub struct User<P: DeMlsProvider, H: GroupEventHandler, SCH: StateChangeHandler> {
    mls_service: MlsService<P::Storage>,
    groups: Arc<RwLock<HashMap<String, GroupEntry>>>,
    consensus_service: Arc<P::Consensus>,
    eth_signer: PrivateKeySigner,
    handler: Arc<H>,
    state_handler: Arc<SCH>,
    /// Default config for new groups (can be overridden per-group).
    default_group_config: GroupConfig,
}

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Create a new User instance with pre-built services and custom default group config.
    ///
    /// # Arguments
    /// * `mls_service` - MLS service for cryptographic operations
    /// * `consensus_service` - Consensus service
    /// * `eth_signer` - Ethereum signer for voting
    /// * `handler` - Event handler for output events
    /// * `state_handler` - Handler for state machine state changes
    /// * `default_group_config` - Default config applied to new groups
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
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `is_creation` - `true` to create a new group as steward, `false` to prepare to join
    pub async fn create_group(
        &mut self,
        group_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        self.create_group_with_config(group_name, is_creation, self.default_group_config.clone())
            .await
    }

    /// Create or join a group with custom config.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `is_creation` - `true` to create a new group as steward, `false` to prepare to join
    /// * `config` - Group-specific configuration
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
            let handle = core::create_group(group_name, &self.mls_service)?;
            let state_machine = GroupStateMachine::new_as_steward_with_config(config);
            (handle, state_machine)
        } else {
            let handle = core::prepare_to_join(group_name);
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
    ///
    /// For `PendingJoin` state: immediate cleanup (no MLS state exists).
    /// For `Leaving` state: error (already leaving).
    /// For `Working`/`Waiting`: transitions to `Leaving` and sends a self-removal
    /// ban request. Actual cleanup happens when the removal commit arrives
    /// via `DispatchAction::LeaveGroup`.
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!("[leave_group]: Leaving group {group_name}");

        let (old_state, new_state) = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            let old_state = entry.state_machine.current_state();
            match old_state {
                GroupState::PendingJoin => {
                    // No MLS state — immediate cleanup
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

        // Notify UI of state change
        self.state_handler
            .on_state_changed(group_name, new_state.clone())
            .await;

        info!(
            "[leave_group]: Transitioning from {old_state} to Leaving, sending self-removal for group {group_name}"
        );

        // Send self-removal directly (bypass process_ban_request's Working-state guard,
        // since we intentionally set Leaving before submitting the removal request).
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

    /// Get epoch history for a group (past batches of approved proposals, most recent last).
    ///
    /// Returns up to the last 10 epoch batches for UI display.
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
    ///
    /// Allowed in `Working` and `Leaving` states (user is still a group member).
    /// Blocked in `PendingJoin` (no MLS state) and `Waiting` (epoch freeze).
    pub async fn send_app_message(
        &self,
        group_name: &str,
        message: Vec<u8>,
    ) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

        // Check if group is in a state where sending is allowed
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
    ///
    /// Returns an error if the group is blocked (PendingJoin, Waiting, or Leaving state).
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
    ) -> Result<(), UserError> {
        // Check if group is in a state where operations are allowed
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
    ///
    /// Called periodically while in PendingJoin state to:
    /// 1. Detect when the member has joined (state changed to Working)
    /// 2. Check for timeout (time-based fallback if group is quiet after rejection)
    ///
    /// Returns `true` if still waiting (PendingJoin), `false` if no longer pending
    /// (either joined, timed out, or group not found).
    pub async fn check_pending_join(&self, group_name: &str) -> bool {
        // First check current state
        let (state, expired) = {
            let groups = self.groups.read().await;
            match groups.get(group_name) {
                Some(entry) => (
                    entry.state_machine.current_state(),
                    entry.state_machine.is_pending_join_expired(),
                ),
                None => return false, // Group already removed
            }
        };

        // If not in PendingJoin, we're done (either joined or left)
        if state != GroupState::PendingJoin {
            return false;
        }

        // Check timeout (time-based fallback)
        if expired {
            info!(
                "[check_pending_join]: Join timed out for group {group_name} \
                 (time-based fallback)"
            );
            self.groups.write().await.remove(group_name);
            let _ = self.handler.on_leave_group(group_name).await;
            return false;
        }

        true // Still waiting
    }

    /// Get the time until the next epoch boundary for a group.
    ///
    /// Returns `None` if the group doesn't exist or hasn't synced yet.
    /// Returns `Some(Duration::ZERO)` if we're already past the boundary.
    pub async fn time_until_next_epoch(&self, group_name: &str) -> Option<std::time::Duration> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name)?;
        entry.state_machine.time_until_next_boundary()
    }

    /// Check if the commit has timed out while in Waiting state.
    ///
    /// Returns a [`CommitTimeoutStatus`] indicating:
    /// - `NotWaiting` — not in Waiting state (nothing to check)
    /// - `StillWaiting` — in Waiting but timeout not reached yet
    /// - `TimedOut { has_proposals }` — timeout reached, state reverted to Working
    ///
    /// When timed out, checks if approved proposals still exist:
    /// - If proposals exist: steward failed to commit (steward fault)
    /// - If no proposals: false alarm (proposals cleared by other means)
    ///
    /// In both cases the member is unblocked (reverted to Working) and the
    /// epoch boundary is re-synced to now.
    pub async fn check_commit_timeout(&self, group_name: &str) -> CommitTimeoutStatus {
        let (has_proposals, violation_epoch, steward_id) = {
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
            let steward_id = entry.handle.steward_identity().unwrap_or_default().to_vec();

            if has_proposals {
                // Steward fault: failed to commit pending proposals within timeout.
                // Clear proposals to break the timeout loop, then file an emergency
                // criteria proposal for censorship/inactivity.
                entry.handle.clear_approved_proposals();
            }

            entry.state_machine.sync_epoch_boundary();
            entry.state_machine.start_working();
            (has_proposals, epoch, steward_id)
        };

        self.state_handler
            .on_state_changed(group_name, GroupState::Working)
            .await;

        // File an emergency criteria proposal for censorship/inactivity.
        // This happens outside the lock since start_voting_on_request_background
        // needs to acquire the groups lock internally.
        if has_proposals {
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
    ///
    /// Non-steward members call this at the epoch boundary (not before).
    /// If they have approved proposals, they transition to Waiting state expecting a commit.
    ///
    /// Returns `true` if entered Waiting state (caller should poll for commit timeout),
    /// `false` otherwise (no polling needed).
    ///
    /// This method does nothing for stewards or members in PendingJoin/Leaving state.
    pub async fn start_member_epoch(&self, group_name: &str) -> Result<bool, UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        // Stewards manage their own epoch
        if entry.state_machine.is_steward() {
            return Ok(false);
        }

        // Skip if not yet joined or leaving
        let state = entry.state_machine.current_state();
        if state == GroupState::PendingJoin || state == GroupState::Leaving {
            return Ok(false);
        }

        // Check if we've reached the epoch boundary
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
        // Check if there are proposals to commit
        let has_proposals = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            core::approved_proposals_count(&entry.handle) > 0
        };

        if !has_proposals {
            return Ok(());
        }

        // Transition to Waiting and notify UI before creating batch
        {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            entry.state_machine.start_steward_epoch()?;
        }
        self.state_handler
            .on_state_changed(group_name, GroupState::Waiting)
            .await;

        // Create and send batch proposals (group is blocked during this)
        let messages = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            create_batch_proposals(&mut entry.handle, &self.mls_service)?
        };

        // TODO: here can be a deadlock if the handler return error, because steward stay in the waiting state
        for message in messages {
            self.handler.on_outbound(group_name, message).await?;
        }

        // Transition back to Working and notify
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
    ///
    /// Allowed in `Working`, `Leaving`, and `PendingJoin` states.
    /// Blocked in `Waiting` state (epoch freeze — vote is sent as MLS message).
    pub async fn process_user_vote(
        &mut self,
        group_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

        // Block voting during Waiting state (epoch freeze)
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
        let (result, handle) = {
            let mut groups = self.groups.write().await;
            let entry = groups
                .get_mut(&group_name)
                .ok_or(UserError::GroupNotFound)?;

            let result = core::process_inbound(
                &mut entry.handle,
                &packet.payload,
                &packet.subtopic,
                &self.mls_service,
            )?;

            (result, entry.handle.clone())
        };

        let action = core::dispatch_result::<P, _>(
            &handle,
            &group_name,
            result,
            &*self.consensus_service,
            &*self.handler,
            &self.mls_service,
        )
        .await?;

        match action {
            core::DispatchAction::StartVoting(request) => {
                self.start_voting_on_request_background(group_name, request)
                    .await?;
            }
            core::DispatchAction::GroupUpdated => {
                // Batch commit received — sync epoch boundary and transition to Working.
                // Skip if in PendingJoin or Leaving (those states have their own flows).
                let transitioned = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(&group_name) {
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
                        .on_state_changed(&group_name, GroupState::Working)
                        .await;
                }
            }
            core::DispatchAction::LeaveGroup => {
                self.groups.write().await.remove(&group_name);
                self.handler.on_leave_group(&group_name).await?;
            }
            core::DispatchAction::JoinedGroup => {
                // Welcome received and joined - sync epoch boundary and transition to Working
                let state = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(&group_name) {
                        entry.state_machine.sync_epoch_boundary();
                        entry.state_machine.start_working();
                        Some(entry.state_machine.current_state())
                    } else {
                        None
                    }
                };
                if let Some(state) = state {
                    self.state_handler
                        .on_state_changed(&group_name, state)
                        .await;
                }
            }
            core::DispatchAction::Done => {}
        }
        Ok(())
    }

    // ─────────────────────────── Consensus Events ───────────────────────────

    /// Handle a consensus event.
    pub async fn handle_consensus_event(
        &mut self,
        group_name: &str,
        event: ConsensusEvent,
    ) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        core::handle_consensus_event::<P>(
            &mut entry.handle,
            group_name,
            event,
            &*self.consensus_service,
        )
        .await?;

        Ok(())
    }
}

// ─────────────────────────── DefaultProvider Convenience ───────────────────────────

impl<H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<DefaultProvider, H, SCH>
{
    /// Convenience constructor for the default provider with default group config.
    ///
    /// Creates a User with MLS service and the given consensus service.
    ///
    /// # Arguments
    /// * `private_key` - Ethereum private key as hex string
    /// * `consensus_service` - The default consensus service
    /// * `handler` - Event handler for output events
    /// * `state_handler` - Handler for state machine state changes
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
    ///
    /// Creates a User with MLS service and the given consensus service.
    ///
    /// # Arguments
    /// * `private_key` - Ethereum private key as hex string
    /// * `consensus_service` - The default consensus service
    /// * `handler` - Event handler for output events
    /// * `state_handler` - Handler for state machine state changes
    /// * `default_group_config` - Default config applied to new groups
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
    /// Returns `true` if this error is fatal and the operation should not be retried.
    ///
    /// Fatal errors indicate the group no longer exists or is in an unrecoverable state.
    /// Non-fatal errors (network issues, temporary failures) can be retried.
    pub fn is_fatal(&self) -> bool {
        matches!(self, UserError::GroupNotFound | UserError::AlreadyLeaving)
    }
}
