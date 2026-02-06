//! Core types for group operations.
//!
//! This module defines the key data types used throughout the DE-MLS core:
//!
//! - [`ProcessResult`] - Outcome of processing an inbound message
//! - [`MessageType`] - Trait for identifying message types
//! - Various `From` implementations for protobuf message conversions

use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    types::ConsensusEvent,
};
use mls_crypto::identity::{normalize_wallet_address, normalize_wallet_address_bytes};
use prost::Message;

use crate::{
    core::CoreError,
    protos::de_mls::messages::v1::{
        app_message, group_update_request, welcome_message, AppMessage, BanRequest,
        BatchProposalsMessage, ConversationMessage, GroupUpdateRequest, InvitationToJoin, Outcome,
        ProposalAdded, RemoveMember, UserKeyPackage, UserVote, VotePayload, WelcomeMessage,
    },
};

// Message type constants for consistency and type safety
pub mod message_types {
    pub const CONVERSATION_MESSAGE: &str = "ConversationMessage";
    pub const BATCH_PROPOSALS_MESSAGE: &str = "BatchProposalsMessage";
    pub const BAN_REQUEST: &str = "BanRequest";
    pub const PROPOSAL: &str = "Proposal";
    pub const VOTE: &str = "Vote";
    pub const VOTE_PAYLOAD: &str = "VotePayload";
    pub const USER_VOTE: &str = "UserVote";
    pub const PROPOSAL_ADDED: &str = "ProposalAdded";
    pub const UNKNOWN: &str = "Unknown";
}

/// Trait for getting message type as a string constant
pub trait MessageType {
    fn message_type(&self) -> &'static str;
}

impl MessageType for app_message::Payload {
    fn message_type(&self) -> &'static str {
        use message_types::*;
        match self {
            app_message::Payload::ConversationMessage(_) => CONVERSATION_MESSAGE,
            app_message::Payload::BatchProposalsMessage(_) => BATCH_PROPOSALS_MESSAGE,
            app_message::Payload::BanRequest(_) => BAN_REQUEST,
            app_message::Payload::Proposal(_) => PROPOSAL,
            app_message::Payload::Vote(_) => VOTE,
            app_message::Payload::VotePayload(_) => VOTE_PAYLOAD,
            app_message::Payload::UserVote(_) => USER_VOTE,
            app_message::Payload::ProposalAdded(_) => PROPOSAL_ADDED,
        }
    }
}

impl MessageType for GroupUpdateRequest {
    fn message_type(&self) -> &'static str {
        match self.payload {
            Some(group_update_request::Payload::InviteMember(_)) => "Add Member",
            Some(group_update_request::Payload::RemoveMember(_)) => "Remove Member",
            _ => "Unknown",
        }
    }
}

/// Result of processing an inbound packet.
///
/// This enum represents all possible outcomes from [`process_inbound`](super::process_inbound).
/// Pass it to [`dispatch_result`](super::dispatch_result) to handle consensus routing
/// and get a [`DispatchAction`](super::DispatchAction) for your application.
///
/// # Variants
///
/// - `AppMessage` - A chat message or other application-level message
/// - `Proposal` / `Vote` - Consensus messages that need forwarding
/// - `GetUpdateRequest` - Steward received a membership change request
/// - `JoinedGroup` - Successfully joined via welcome message
/// - `GroupUpdated` - MLS state changed (batch commit applied)
/// - `LeaveGroup` - User was removed from the group
/// - `Noop` - Nothing to do (message not for us, already processed, etc.)
#[derive(Debug, Clone)]
pub enum ProcessResult {
    /// An application message was received (chat message, etc.).
    ///
    /// The message has been decrypted and is ready for display.
    AppMessage(AppMessage),

    /// A consensus proposal was received from another peer.
    ///
    /// Should be forwarded to the consensus service via
    /// [`forward_incoming_proposal`](super::forward_incoming_proposal).
    Proposal(Proposal),

    /// A consensus vote was received from another peer.
    ///
    /// Should be forwarded to the consensus service via
    /// [`forward_incoming_vote`](super::forward_incoming_vote).
    Vote(Vote),

    /// The user was removed from the group.
    ///
    /// Application should clean up group state and notify the UI.
    LeaveGroup,

    /// Steward received a membership change request (key package or ban).
    ///
    /// Application should start a consensus vote for this request.
    GetUpdateRequest(GroupUpdateRequest),

    /// The user successfully joined a group via welcome message.
    ///
    /// Contains the group name. Application should transition state
    /// from PendingJoin to Working.
    JoinedGroup(String),

    /// Group MLS state was updated (batch commit applied).
    ///
    /// Application should transition state from Waiting to Working.
    GroupUpdated,

    /// No action needed.
    ///
    /// The message was not for us, was a duplicate, or required no action.
    Noop,
}

// WELCOME MESSAGE SUBTOPIC

pub fn invitation_from_bytes(mls_bytes: Vec<u8>) -> WelcomeMessage {
    let invitation = InvitationToJoin {
        mls_message_out_bytes: mls_bytes,
    };

    WelcomeMessage {
        payload: Some(welcome_message::Payload::InvitationToJoin(invitation)),
    }
}

impl From<UserKeyPackage> for WelcomeMessage {
    fn from(user_key_package: UserKeyPackage) -> Self {
        WelcomeMessage {
            payload: Some(welcome_message::Payload::UserKeyPackage(user_key_package)),
        }
    }
}

// APPLICATION MESSAGE SUBTOPIC

impl From<VotePayload> for AppMessage {
    fn from(vote_payload: VotePayload) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::VotePayload(vote_payload)),
        }
    }
}

impl From<UserVote> for AppMessage {
    fn from(user_vote: UserVote) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::UserVote(user_vote)),
        }
    }
}

impl From<ConversationMessage> for AppMessage {
    fn from(conversation_message: ConversationMessage) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::ConversationMessage(
                conversation_message,
            )),
        }
    }
}

impl From<BatchProposalsMessage> for AppMessage {
    fn from(batch_proposals_message: BatchProposalsMessage) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::BatchProposalsMessage(
                batch_proposals_message,
            )),
        }
    }
}

impl From<BanRequest> for AppMessage {
    fn from(ban_request: BanRequest) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::BanRequest(ban_request)),
        }
    }
}

impl From<Proposal> for AppMessage {
    fn from(proposal: Proposal) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::Proposal(proposal)),
        }
    }
}

impl From<Vote> for AppMessage {
    fn from(vote: Vote) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::Vote(vote)),
        }
    }
}

impl From<ProposalAdded> for AppMessage {
    fn from(proposal_added: ProposalAdded) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::ProposalAdded(proposal_added)),
        }
    }
}

impl From<ConsensusEvent> for Outcome {
    fn from(consensus_event: ConsensusEvent) -> Self {
        match consensus_event {
            ConsensusEvent::ConsensusReached {
                proposal_id: _,
                result: true,
                timestamp: _,
            } => Outcome::Accepted,
            ConsensusEvent::ConsensusReached {
                proposal_id: _,
                result: false,
                timestamp: _,
            } => Outcome::Rejected,
            ConsensusEvent::ConsensusFailed {
                proposal_id: _,
                timestamp: _,
            } => Outcome::Unspecified,
        }
    }
}

impl TryFrom<AppMessage> for ProcessResult {
    type Error = CoreError;
    fn try_from(value: AppMessage) -> Result<Self, Self::Error> {
        match &value.payload {
            Some(app_message::Payload::ConversationMessage(_)) => {
                Ok(ProcessResult::AppMessage(value))
            }
            Some(app_message::Payload::Proposal(proposal)) => {
                Ok(ProcessResult::Proposal(proposal.clone()))
            }
            Some(app_message::Payload::Vote(vote)) => Ok(ProcessResult::Vote(vote.clone())),
            Some(app_message::Payload::BanRequest(ban_request)) => {
                Ok(ProcessResult::GetUpdateRequest(GroupUpdateRequest {
                    payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                        identity: normalize_wallet_address_bytes(ban_request.user_to_ban.as_str())?,
                    })),
                }))
            }
            _ => Ok(ProcessResult::Noop),
        }
    }
}

// Helper function to convert protobuf GroupUpdateRequest to display format
pub fn convert_group_request_to_display(request: Vec<u8>) -> (String, String) {
    let request = GroupUpdateRequest::decode(request.as_slice()).unwrap_or_default();
    match request.payload {
        Some(group_update_request::Payload::InviteMember(im)) => (
            "Add Member".to_string(),
            normalize_wallet_address(&im.identity),
        ),
        Some(group_update_request::Payload::RemoveMember(rm)) => (
            "Remove Member".to_string(),
            normalize_wallet_address(&rm.identity),
        ),
        _ => ("Unknown".to_string(), "Invalid request".to_string()),
    }
}

pub fn get_identity_from_group_update_request(req: GroupUpdateRequest) -> String {
    match req.payload {
        Some(group_update_request::Payload::InviteMember(im)) => {
            normalize_wallet_address(&im.identity)
        }
        Some(group_update_request::Payload::RemoveMember(rm)) => {
            normalize_wallet_address(&rm.identity)
        }
        _ => "unknown".to_string(),
    }
}
