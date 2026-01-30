//! User struct for managing multiple groups.
//!
//! This is the main entry point for the application layer,
//! managing multiple `GroupHandle`s and coordinating operations.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::signers::local::{LocalSignerError, PrivateKeySigner};
use ds::transport::{InboundPacket, OutboundPacket};
use hashgraph_like_consensus::api::ConsensusServiceAPI;
use hashgraph_like_consensus::protos::consensus::v1::Proposal;
use hashgraph_like_consensus::service::DefaultConsensusService;
use hashgraph_like_consensus::session::ConsensusConfig;
use hashgraph_like_consensus::types::{ConsensusEvent, CreateProposalRequest};
use mls_crypto::{IdentityService, OpenMlsIdentityService};
use prost::Message;
use tokio::sync::RwLock;
use tracing::{error, info};

use super::consensus_handler::ConsensusHandler;
use super::pending_batches::PendingBatches;
use super::state_machine::{GroupState, GroupStateMachine};
use crate::core::{self, CoreError, DeMlsProvider, DefaultProvider, GroupHandle, ProcessResult};
use crate::protos::de_mls::messages::v1::{
    AppMessage, BanRequest, ConversationMessage, ProposalAdded, UpdateRequest, UpdateRequestList,
    VotePayload,
};

/// Represents the action to take after processing a user message or event.
#[derive(Debug, Clone)]
pub enum UserAction {
    /// Send an outbound packet.
    Outbound(OutboundPacket),
    /// Send an application message to the UI.
    SendToApp(AppMessage),
    /// Leave a group.
    LeaveGroup(String),
    /// No action needed.
    DoNothing,
}

impl std::fmt::Display for UserAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserAction::Outbound(_) => write!(f, "Outbound"),
            UserAction::SendToApp(_) => write!(f, "SendToApp"),
            UserAction::LeaveGroup(name) => write!(f, "LeaveGroup({name})"),
            UserAction::DoNothing => write!(f, "DoNothing"),
        }
    }
}

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
pub struct User<P: DeMlsProvider> {
    identity_service: P::Identity,
    groups: Arc<RwLock<HashMap<String, GroupEntry>>>,
    consensus_service: Arc<P::Consensus>,
    eth_signer: PrivateKeySigner,
    pending_batch_proposals: PendingBatches,
}

impl<P: DeMlsProvider> User<P> {
    /// Create a new User instance with pre-built services.
    ///
    /// # Arguments
    /// * `identity_service` - Identity and MLS service
    /// * `consensus_service` - Consensus service
    /// * `eth_signer` - Ethereum signer for voting
    pub fn new(
        identity_service: P::Identity,
        consensus_service: Arc<P::Consensus>,
        eth_signer: PrivateKeySigner,
    ) -> Self {
        Self {
            identity_service,
            groups: Arc::new(RwLock::new(HashMap::new())),
            consensus_service,
            eth_signer,
            pending_batch_proposals: PendingBatches::new(),
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
            let state_machine = GroupStateMachine::new();
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
    pub async fn get_current_epoch_proposals(
        &self,
        group_name: &str,
    ) -> Result<Vec<crate::core::GroupUpdateRequest>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        Ok(core::approved_proposals(&entry.handle))
    }

    // ─────────────────────────── Messaging ───────────────────────────

    /// Build and return a message for a group.
    pub async fn build_group_message(
        &self,
        group_name: &str,
        app_msg: AppMessage,
    ) -> Result<OutboundPacket, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let packet = core::build_message(&entry.handle, &self.identity_service, &app_msg).await?;
        Ok(packet)
    }

    /// Build and return a key package message for a group.
    pub async fn build_key_package_message(
        &mut self,
        group_name: &str,
    ) -> Result<OutboundPacket, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let packet = core::build_key_package_message(&entry.handle, &mut self.identity_service)?;
        Ok(packet)
    }

    /// Send a conversation message to a group.
    pub async fn send_message(
        &self,
        group_name: &str,
        message: Vec<u8>,
    ) -> Result<OutboundPacket, UserError> {
        tracing::info!("sending message to group: {:?}", &group_name);
        let app_msg: AppMessage = ConversationMessage {
            message,
            sender: self.identity_string(),
            group_name: group_name.to_string(),
        }
        .into();

        tracing::info!("built app message: {:?}", &app_msg);
        self.build_group_message(group_name, app_msg).await
    }

    /// Process a ban request.
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        let normalized = mls_crypto::normalize_wallet_address_str(&ban_request.user_to_ban)?;
        info!("[process_ban_request]: Processing ban request for user {normalized}");

        if entry.state_machine.is_steward() {
            info!("[process_ban_request]: Steward adding remove proposal");
            if let Some(request) = entry.handle.store_remove_proposal(normalized.clone()) {
                let msg: AppMessage = ProposalAdded {
                    group_id: group_name.to_string(),
                    request: Some(request),
                }
                .into();
                return Ok(UserAction::SendToApp(msg));
            }
        }

        // Non-steward: forward to group
        let updated_request = BanRequest {
            user_to_ban: normalized,
            requester: self.identity_string(),
            group_name: ban_request.group_name,
        };
        drop(groups);
        let packet = self
            .build_group_message(group_name, updated_request.into())
            .await?;
        Ok(UserAction::Outbound(packet))
    }

    // ─────────────────────────── Steward Operations ───────────────────────────

    /// Start a steward epoch.
    ///
    /// Returns the number of proposals (0 if no proposals).
    pub async fn start_steward_epoch(&mut self, group_name: &str) -> Result<usize, UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        let proposal_count = core::approved_proposals_count(&entry.handle);
        let count = entry.state_machine.start_steward_epoch(proposal_count)?;

        if count > 0 {
            entry.handle.start_voting_epoch();
            info!("[start_steward_epoch]: Started with {count} proposals");
        } else {
            info!("[start_steward_epoch]: No proposals, staying in Working");
        }

        Ok(count)
    }

    /// Get proposals for steward voting and start voting.
    ///
    /// Returns (proposal_id, UserAction).
    pub async fn get_proposals_for_steward_voting(
        &mut self,
        group_name: &str,
    ) -> Result<(u32, UserAction), UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        if !entry.state_machine.is_steward() {
            info!("[get_proposals_for_steward_voting]: Not steward");
            return Ok((0, UserAction::DoNothing));
        }

        let proposals: Vec<UpdateRequest> = entry
            .handle
            .voting_proposals()
            .into_iter()
            .map(|p| p.into())
            .collect();

        if proposals.is_empty() {
            error!("[get_proposals_for_steward_voting]: No proposals found");
            return Err(UserError::NoProposals);
        }

        entry.state_machine.start_voting()?;

        // Get member count for expected voters
        let members = core::group_members(&entry.handle, &self.identity_service).await?;
        let expected_voters = members.len() as u32;

        let payload = UpdateRequestList {
            update_requests: proposals.clone(),
        }
        .encode_to_vec();

        let request = CreateProposalRequest::new(
            uuid::Uuid::new_v4().to_string(),
            payload,
            self.identity_service.identity_string().into(),
            expected_voters,
            3600,
            true,
        )?;

        let scope = P::Scope::from(group_name.to_string());
        let proposal = self
            .consensus_service
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
            group_id: group_name.to_string(),
            proposal_id: proposal.proposal_id,
            group_requests: proposals,
            timestamp: proposal.timestamp,
        }
        .into();

        Ok((proposal.proposal_id, UserAction::SendToApp(vote_payload)))
    }

    // ─────────────────────────── Voting ───────────────────────────

    /// Process a user vote.
    pub async fn process_user_vote(
        &mut self,
        group_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<UserAction, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let is_steward = entry.state_machine.is_steward();
        drop(groups);

        let scope = P::Scope::from(group_name.to_string());
        let app_message: AppMessage = if is_steward {
            info!("[process_user_vote]: Steward voting on proposal {proposal_id}");
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
        Ok(UserAction::Outbound(packet))
    }

    // ─────────────────────────── Inbound Processing ───────────────────────────

    /// Process an inbound packet.
    pub async fn process_inbound_packet(
        &mut self,
        packet: InboundPacket,
    ) -> Result<UserAction, UserError> {
        let group_name = packet.group_id.clone();

        // Check if message is from same app instance
        {
            let groups = self.groups.read().await;
            if let Some(entry) = groups.get(&group_name) {
                if packet.app_id == entry.handle.app_id() {
                    return Ok(UserAction::DoNothing);
                }
            } else {
                return Err(UserError::GroupNotFound);
            }
        }

        // Process the packet
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

        self.handle_process_result(&group_name, result, entry).await
    }

    async fn handle_process_result(
        &self,
        group_name: &str,
        result: ProcessResult,
        entry: &mut GroupEntry,
    ) -> Result<UserAction, UserError> {
        match result {
            ProcessResult::AppMessage(msg) => Ok(UserAction::SendToApp(msg)),
            ProcessResult::LeaveGroup => Ok(UserAction::LeaveGroup(group_name.to_string())),
            ProcessResult::Proposal(proposal) => {
                self.process_incoming_proposal(group_name, proposal, entry)
                    .await
            }
            ProcessResult::Vote(vote) => {
                let scope = P::Scope::from(group_name.to_string());
                self.consensus_service
                    .process_incoming_vote(&scope, vote)
                    .await?;
                Ok(UserAction::DoNothing)
            }
            ProcessResult::MemberProposalAdded(request) => {
                let msg: AppMessage = ProposalAdded {
                    group_id: group_name.to_string(),
                    request: Some(request),
                }
                .into();
                Ok(UserAction::SendToApp(msg))
            }
            ProcessResult::JoinedGroup(name) => {
                let msg: AppMessage = ConversationMessage {
                    message: format!("User {} joined the group", self.identity_string())
                        .into_bytes(),
                    sender: "SYSTEM".to_string(),
                    group_name: name,
                }
                .into();

                let packet =
                    core::build_message(&entry.handle, &self.identity_service, &msg).await?;
                Ok(UserAction::Outbound(packet))
            }
            ProcessResult::Noop => Ok(UserAction::DoNothing),
        }
    }

    async fn process_incoming_proposal(
        &self,
        group_name: &str,
        proposal: Proposal,
        entry: &mut GroupEntry,
    ) -> Result<UserAction, UserError> {
        let scope = P::Scope::from(group_name.to_string());
        self.consensus_service
            .process_incoming_proposal(&scope, proposal.clone())
            .await?;

        entry.state_machine.start_voting()?;
        info!(
            "[process_incoming_proposal]: Starting voting for proposal {}",
            proposal.proposal_id
        );

        let update_request_list = UpdateRequestList::decode(proposal.payload.as_slice())?;

        let vote_payload: AppMessage = VotePayload {
            group_id: group_name.to_string(),
            proposal_id: proposal.proposal_id,
            group_requests: update_request_list.update_requests,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        }
        .into();

        Ok(UserAction::SendToApp(vote_payload))
    }

    // ─────────────────────────── Consensus Events ───────────────────────────

    /// Handle a consensus event.
    ///
    /// Returns outbound packets that need to be sent via the delivery service.
    pub async fn handle_consensus_event(
        &mut self,
        group_name: &str,
        event: ConsensusEvent,
    ) -> Result<Vec<OutboundPacket>, UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        let messages = ConsensusHandler::handle_event(
            &mut entry.handle,
            &mut entry.state_machine,
            event,
            &self.identity_service,
        )
        .await?;

        // Check for pending batch proposals after consensus
        if entry.state_machine.current_state() == GroupState::Waiting
            && self.pending_batch_proposals.contains(group_name).await
        {
            if let Some(batch) = self.pending_batch_proposals.take(group_name).await {
                info!("[handle_consensus_event]: Processing stored batch proposals");
                let result = crate::core::process_inbound(
                    &mut entry.handle,
                    &batch.commit_message,
                    ds::APP_MSG_SUBTOPIC,
                    &self.identity_service,
                    &self.identity_service,
                )
                .await?;

                entry.state_machine.start_working();

                if let ProcessResult::LeaveGroup = result {
                    // Will need to handle this at a higher level
                }
            }
        }

        Ok(messages)
    }
}

// ─────────────────────────── DefaultProvider Convenience ───────────────────────────

impl User<DefaultProvider> {
    /// Convenience constructor for the default provider.
    ///
    /// Creates a User with OpenMLS identity and the given consensus service.
    ///
    /// # Arguments
    /// * `private_key` - Ethereum private key as hex string
    /// * `consensus_service` - The default consensus service
    pub fn with_private_key(
        private_key: &str,
        consensus_service: Arc<DefaultConsensusService>,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = signer.address();
        let identity_service = OpenMlsIdentityService::new(user_address.as_slice())?;
        Ok(Self::new(identity_service, consensus_service, signer))
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

    #[error("No proposals found")]
    NoProposals,

    #[error("Core error: {0}")]
    Core(#[from] CoreError),

    #[error("State machine error: {0}")]
    StateMachine(#[from] super::state_machine::StateMachineError),

    #[error("Consensus handler error: {0}")]
    ConsensusHandler(#[from] super::consensus_handler::ConsensusHandlerError),

    #[error("MLS error: {0}")]
    Mls(#[from] mls_crypto::MlsServiceError),

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

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
