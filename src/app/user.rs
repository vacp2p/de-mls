//! User struct for managing multiple groups.
//!
//! This is the main entry point for the application layer,
//! managing multiple `GroupHandle`s and coordinating operations.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::signers::local::{LocalSignerError, PrivateKeySigner};
use ds::transport::InboundPacket;
use hashgraph_like_consensus::api::ConsensusServiceAPI;
use hashgraph_like_consensus::protos::consensus::v1::Proposal;
use hashgraph_like_consensus::service::DefaultConsensusService;
use hashgraph_like_consensus::session::ConsensusConfig;
use hashgraph_like_consensus::types::{ConsensusEvent, CreateProposalRequest};
use mls_crypto::identity::normalize_wallet_address_bytes;
use mls_crypto::{IdentityService, OpenMlsIdentityService};
use prost::Message;
use tokio::sync::RwLock;
use tracing::{error, info};

use super::state_machine::{GroupState, GroupStateMachine};
use crate::core::{
    self, create_batch_proposals, CoreError, DeMlsProvider, DefaultProvider, GroupEventHandler,
    GroupHandle, ProcessResult,
};
use crate::protos::de_mls::messages::v1::{
    group_update_request, AppMessage, BanRequest, ConversationMessage, GroupUpdateRequest,
    RemoveMember, VotePayload,
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
/// (identity, MLS, consensus). Use [`DefaultProvider`] for standard configuration.
///
/// The type parameter `H` is the handler that receives output events
/// (outbound packets, app messages, leave/join notifications).
pub struct User<P: DeMlsProvider, H: GroupEventHandler> {
    identity_service: P::Identity,
    groups: Arc<RwLock<HashMap<String, GroupEntry>>>,
    consensus_service: Arc<P::Consensus>,
    eth_signer: PrivateKeySigner,
    handler: Arc<H>,
}

impl<P: DeMlsProvider, H: GroupEventHandler + 'static> User<P, H> {
    /// Create a new User instance with pre-built services.
    ///
    /// # Arguments
    /// * `identity_service` - Identity and MLS service
    /// * `consensus_service` - Consensus service
    /// * `eth_signer` - Ethereum signer for voting
    /// * `handler` - Event handler for output events
    fn new(
        identity_service: P::Identity,
        consensus_service: Arc<P::Consensus>,
        eth_signer: PrivateKeySigner,
        handler: Arc<H>,
    ) -> Self {
        Self {
            identity_service,
            groups: Arc::new(RwLock::new(HashMap::new())),
            consensus_service,
            eth_signer,
            handler,
        }
    }

    /// Get the user's identity string.
    pub fn identity_string(&self) -> String {
        self.identity_service.identity_string()
    }

    // ─────────────────────────── Group Management ───────────────────────────

    /// Create or join a group.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `is_creation` - `true` to create a new group as steward, `false` to prepare to join
    pub async fn create_group(
        &mut self,
        group_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        if groups.contains_key(group_name) {
            return Err(UserError::GroupAlreadyExists);
        }

        let (handle, state_machine) = if is_creation {
            let handle = core::create_group(group_name, &self.identity_service)?;
            let state_machine = GroupStateMachine::new_as_steward();
            (handle, state_machine)
        } else {
            let handle = core::prepare_to_join(group_name);
            let state_machine = GroupStateMachine::new_as_member();
            (handle, state_machine)
        };

        groups.insert(
            group_name.to_string(),
            GroupEntry {
                handle,
                state_machine,
            },
        );
        Ok(())
    }

    /// Leave a group.
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!("[leave_group]: Leaving group {group_name}");
        self.groups.write().await.remove(group_name);
        Ok(())
    }

    /// Get the state of a group.
    pub async fn get_group_state(&self, group_name: &str) -> Result<GroupState, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        Ok(entry.state_machine.current_state())
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

        let members = core::group_members(&entry.handle, &self.identity_service).await?;
        Ok(members
            .into_iter()
            .map(|raw| mls_crypto::identity::normalize_wallet_address(raw.as_slice()).to_string())
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

    /// Build and return a message for a group.
    async fn build_group_message(
        &self,
        group_name: &str,
        app_msg: AppMessage,
    ) -> Result<ds::transport::OutboundPacket, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let packet = core::build_message(&entry.handle, &self.identity_service, &app_msg).await?;
        Ok(packet)
    }

    /// Build and send a key package message for a group via the handler.
    pub async fn send_kp_message(&mut self, group_name: &str) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let packet = core::build_key_package_message(&entry.handle, &mut self.identity_service)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Send a conversation message to a group.
    pub async fn send_app_message(
        &self,
        group_name: &str,
        message: Vec<u8>,
    ) -> Result<(), UserError> {
        let app_msg: AppMessage = ConversationMessage {
            message,
            sender: self.identity_string(),
            group_name: group_name.to_string(),
        }
        .into();

        let packet = self.build_group_message(group_name, app_msg).await?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Process a ban request.
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
    ) -> Result<(), UserError> {
        self.start_voting_on_request_background(
            group_name.to_string(),
            GroupUpdateRequest {
                payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                    identity: normalize_wallet_address_bytes(ban_request.user_to_ban.as_str())?,
                })),
            },
        )
        .await?;

        Ok(())
    }

    // ─────────────────────────── Steward Operations ───────────────────────────

    /// Start a member epoch check.
    ///
    /// Non-steward members call this on the same interval as the steward epoch.
    /// If they have approved proposals, they transition to Waiting state,
    /// expecting to receive a batch proposal from the steward.
    pub async fn start_member_epoch(&self, group_name: &str) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        if entry.state_machine.is_steward() {
            return Ok(());
        }

        let proposal_count = core::approved_proposals_count(&entry.handle);
        if proposal_count > 0 && entry.state_machine.current_state() == GroupState::Working {
            entry.state_machine.start_waiting();
            info!(
                "[start_member_epoch]: Member entering Waiting state for group {group_name:?} \
                 ({proposal_count} approved proposals pending)"
            );
        }

        Ok(())
    }

    /// Start a steward epoch.
    pub async fn start_steward_epoch(&mut self, group_name: &str) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        let proposal_count = core::approved_proposals_count(&entry.handle);
        if proposal_count > 0 {
            entry.state_machine.start_steward_epoch()?;
            // create_batch_proposals clears approved proposals internally (archiving to history)
            let messages =
                create_batch_proposals(&mut entry.handle, &self.identity_service).await?;
            for message in messages {
                self.handler.on_outbound(group_name, message).await?;
            }
            entry.state_machine.start_working();
        }

        Ok(())
    }

    pub async fn start_voting_on_request_background(
        &self,
        group_name: String,
        upd_request: GroupUpdateRequest,
    ) -> Result<(), UserError> {
        let handle = {
            let groups = self.groups.read().await;
            let entry = groups.get(&group_name).ok_or(UserError::GroupNotFound)?;
            entry.handle.clone()
        };
        let members = core::group_members(&handle, &self.identity_service).await?;
        let expected_voters = members.len() as u32;
        let payload = upd_request.encode_to_vec();
        let identity = self.identity_service.identity_string();

        let consensus = Arc::clone(&self.consensus_service);
        let groups = Arc::clone(&self.groups);
        let handler = Arc::clone(&self.handler);

        tokio::spawn(async move {
            let result: Result<(), UserError> = async {
                info!("[start_voting_on_request]: Expected voters: {expected_voters}");

                let request = CreateProposalRequest::new(
                    uuid::Uuid::new_v4().to_string(),
                    payload.clone(),
                    identity.into(),
                    expected_voters,
                    3600,
                    true,
                )?;


                let scope = P::Scope::from(group_name.clone());
                let proposal = consensus
                    .create_proposal_with_config(
                        &scope,
                        request,
                        Some(ConsensusConfig::gossipsub().with_timeout(Duration::from_secs(15))?),
                    )
                    .await?;

                info!(
                    "[get_proposals_for_steward_voting]: Created proposal {} with {} expected voters",
                    proposal.proposal_id, expected_voters
                );

                let vote_payload: AppMessage = VotePayload {
                    group_id: group_name.clone(),
                    proposal_id: proposal.proposal_id,
                    payload,
                    timestamp: proposal.timestamp,
                }
                .into();

                handler
                    .on_app_message(&group_name, vote_payload)
                    .await?;

                {
                    let mut groups = groups.write().await;
                    if let Some(entry) = groups.get_mut(&group_name) {
                        entry
                            .handle
                            .store_voting_proposal(proposal.proposal_id, upd_request);
                    } else {
                        error!(
                            "[start_voting_on_request]: Group {group_name} missing during proposal store"
                        );
                    }
                }

                info!(
                    "[start_voting_on_request]: Stored voting proposal: {}",
                    proposal.proposal_id
                );

                Ok(())
            }
            .await;

            if let Err(err) = result {
                error!("[start_voting_on_request]: background task failed: {err}");
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
        let is_owner = entry.handle.is_owner_of_proposal(proposal_id);
        drop(groups);

        let scope = P::Scope::from(group_name.to_string());
        let app_message: AppMessage = if is_owner {
            info!("[process_user_vote]: Owner voting on proposal {proposal_id}");
            let proposal = self
                .consensus_service
                .cast_vote_and_get_proposal(&scope, proposal_id, vote, self.eth_signer.clone())
                .await?;
            proposal.into()
        } else {
            info!("[process_user_vote]: User voting on proposal {proposal_id}");
            let vote_msg = self
                .consensus_service
                .cast_vote(&scope, proposal_id, vote, self.eth_signer.clone())
                .await?;
            vote_msg.into()
        };

        let packet = self.build_group_message(group_name, app_message).await?;
        self.handler.on_outbound(group_name, packet).await?;
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
        let (result, joined_handle) = {
            let mut groups = self.groups.write().await;
            let entry = groups
                .get_mut(&group_name)
                .ok_or(UserError::GroupNotFound)?;

            let result = core::process_inbound(
                &mut entry.handle,
                &packet.payload,
                &packet.subtopic,
                &self.identity_service,
                &self.identity_service,
            )
            .await?;

            let joined_handle = match &result {
                ProcessResult::JoinedGroup(_) => Some(entry.handle.clone()),
                _ => None,
            };

            (result, joined_handle)
        };

        match result {
            ProcessResult::AppMessage(msg) => {
                self.handler
                    .on_app_message(group_name.as_str(), msg)
                    .await?;
            }
            ProcessResult::LeaveGroup => {
                self.handler.on_leave_group(group_name.as_str()).await?;
            }
            ProcessResult::Proposal(proposal) => {
                self.process_incoming_proposal(group_name.as_str(), proposal)
                    .await?;
            }
            ProcessResult::Vote(vote) => {
                let scope = P::Scope::from(group_name.to_string());
                self.consensus_service
                    .process_incoming_vote(&scope, vote)
                    .await?;
            }
            ProcessResult::GetUpdateRequest(request) => {
                self.start_voting_on_request_background(group_name.to_string(), request)
                    .await?;
            }
            ProcessResult::JoinedGroup(name) => {
                let handle = joined_handle.ok_or(UserError::GroupNotFound)?;
                let msg: AppMessage = ConversationMessage {
                    message: format!("User {} joined the group", self.identity_string())
                        .into_bytes(),
                    sender: "SYSTEM".to_string(),
                    group_name: name.clone(),
                }
                .into();

                let packet = core::build_message(&handle, &self.identity_service, &msg).await?;
                self.handler.on_outbound(&name, packet).await?;
                self.handler.on_joined_group(&name).await?;
            }
            ProcessResult::GroupUpdated => {
                let mut groups = self.groups.write().await;
                if let Some(entry) = groups.get_mut(&group_name) {
                    entry.state_machine.start_working();
                }
            }
            ProcessResult::Noop => {}
        }
        Ok(())
    }

    async fn process_incoming_proposal(
        &self,
        group_name: &str,
        proposal: Proposal,
    ) -> Result<(), UserError> {
        let scope = P::Scope::from(group_name.to_string());
        self.consensus_service
            .process_incoming_proposal(&scope, proposal.clone())
            .await?;

        info!(
            "[process_incoming_proposal]: Starting voting for proposal {}",
            proposal.proposal_id
        );

        let vote_payload: AppMessage = VotePayload {
            group_id: group_name.to_string(),
            proposal_id: proposal.proposal_id,
            payload: proposal.payload.clone(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        }
        .into();

        self.handler
            .on_app_message(group_name, vote_payload)
            .await?;
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

        match event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
                timestamp: _,
            } => {
                info!("Consensus reached for proposal {proposal_id}: result={result}");
                let is_owner = entry.handle.is_owner_of_proposal(proposal_id);
                if result && is_owner {
                    entry.handle.mark_proposal_as_approved(proposal_id);
                } else if !result && is_owner {
                    entry.handle.mark_proposal_as_rejected(proposal_id);
                } else if result && !is_owner {
                    let payload = self
                        .consensus_service
                        .get_proposal_payload(&P::Scope::from(group_name.to_string()), proposal_id)
                        .await?;
                    let update_request = GroupUpdateRequest::decode(payload.as_slice())?;
                    entry
                        .handle
                        .insert_approved_proposal(proposal_id, update_request);
                }
            }
            ConsensusEvent::ConsensusFailed {
                proposal_id,
                timestamp: _,
            } => {
                info!("Consensus failed for proposal {proposal_id}");
                entry.handle.mark_proposal_as_rejected(proposal_id);
            }
        }

        Ok(())
    }
}

// ─────────────────────────── DefaultProvider Convenience ───────────────────────────

impl<H: GroupEventHandler + 'static> User<DefaultProvider, H> {
    /// Convenience constructor for the default provider.
    ///
    /// Creates a User with OpenMLS identity and the given consensus service.
    ///
    /// # Arguments
    /// * `private_key` - Ethereum private key as hex string
    /// * `consensus_service` - The default consensus service
    /// * `handler` - Event handler for output events
    pub fn with_private_key(
        private_key: &str,
        consensus_service: Arc<DefaultConsensusService>,
        handler: Arc<H>,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = signer.address();
        let identity_service = OpenMlsIdentityService::new(user_address.as_slice())?;
        Ok(Self::new(
            identity_service,
            consensus_service,
            signer,
            handler,
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
    Identity(#[from] mls_crypto::error::IdentityError),
}
