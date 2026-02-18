//! Core types for group operations.
//!
//! This module defines the key data types used throughout the DE-MLS core:
//!
//! - [`ProcessResult`] - Outcome of processing an inbound message
//! - [`MessageType`] - Trait for identifying message types
//! - Various `From` implementations for protobuf message conversions

use prost::Message;

use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    types::ConsensusEvent,
};

use crate::{
    core::CoreError,
    mls_crypto::{format_wallet_address, parse_wallet_to_bytes},
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, BatchProposalsMessage, ConversationMessage, GroupUpdateRequest,
        InvitationToJoin, Outcome, ProposalAdded, RemoveMember, UserKeyPackage, UserVote,
        ViolationEvidence, VotePayload, WelcomeMessage, app_message, group_update_request,
        welcome_message,
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
        match &self.payload {
            Some(group_update_request::Payload::InviteMember(_)) => "Add Member",
            Some(group_update_request::Payload::RemoveMember(_)) => "Remove Member",
            Some(group_update_request::Payload::EmergencyCriteria(ec)) => ec
                .evidence
                .as_ref()
                .map(|e| match ViolationType::try_from(e.violation_type) {
                    Ok(ViolationType::BrokenCommit) => "Emergency: Broken Commit",
                    Ok(ViolationType::BrokenMlsProposal) => "Emergency: Broken MLS Proposal",
                    Ok(ViolationType::CensorshipInactivity) => "Emergency: Censorship/Inactivity",
                    _ => "Emergency: Unknown Violation",
                })
                .unwrap_or("Emergency: Unknown Violation"),
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

    /// A steward violation was detected during commit validation.
    ///
    /// Contains evidence of the violation. The application should start
    /// an emergency criteria proposal vote for this evidence.
    ViolationDetected(ViolationEvidence),

    /// No action needed.
    ///
    /// The message was not for us, was a duplicate, or required no action.
    Noop,
}

// ── ViolationEvidence constructors ────────────────────────────────

use crate::protos::de_mls::messages::v1::{EmergencyCriteriaProposal, ViolationType};

impl ViolationEvidence {
    /// Steward included different proposal IDs than what was voted on,
    /// or IDs match but content digest differs.
    pub fn broken_commit(target: Vec<u8>, epoch: u64, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            violation_type: ViolationType::BrokenCommit as i32,
            target_member_id: target,
            evidence_payload: payload.into(),
            epoch,
        }
    }

    /// MLS payload count doesn't match proposal count,
    /// or an MLS proposal failed to decrypt/store correctly.
    pub fn broken_mls_proposal(target: Vec<u8>, epoch: u64, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            violation_type: ViolationType::BrokenMlsProposal as i32,
            target_member_id: target,
            evidence_payload: payload.into(),
            epoch,
        }
    }

    /// Steward didn't commit within the threshold duration.
    pub fn censorship_inactivity(target: Vec<u8>, epoch: u64) -> Self {
        Self {
            violation_type: ViolationType::CensorshipInactivity as i32,
            target_member_id: target,
            evidence_payload: Vec::new(),
            epoch,
        }
    }

    /// Human-readable label for the violation type.
    pub fn violation_type_label(&self) -> &'static str {
        match ViolationType::try_from(self.violation_type) {
            Ok(ViolationType::BrokenCommit) => "Broken Commit",
            Ok(ViolationType::BrokenMlsProposal) => "Broken MLS Proposal",
            Ok(ViolationType::CensorshipInactivity) => "Censorship/Inactivity",
            _ => "Unknown Violation",
        }
    }

    /// Wrap this evidence into a `GroupUpdateRequest` for consensus voting.
    pub fn into_update_request(self) -> GroupUpdateRequest {
        GroupUpdateRequest {
            payload: Some(group_update_request::Payload::EmergencyCriteria(
                EmergencyCriteriaProposal {
                    evidence: Some(self),
                },
            )),
        }
    }
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
                        identity: parse_wallet_to_bytes(ban_request.user_to_ban.as_str())?,
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
            format_wallet_address(&im.identity),
        ),
        Some(group_update_request::Payload::RemoveMember(rm)) => (
            "Remove Member".to_string(),
            format_wallet_address(&rm.identity),
        ),
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => {
            let (label, target) = match ec.evidence.as_ref() {
                Some(e) => (
                    format!("Emergency: {}", e.violation_type_label()),
                    format_wallet_address(&e.target_member_id),
                ),
                None => (
                    "Emergency: Unknown Violation".to_string(),
                    "unknown".to_string(),
                ),
            };
            (label, target)
        }
        _ => ("Unknown".to_string(), "Invalid request".to_string()),
    }
}

pub fn get_identity_from_group_update_request(req: GroupUpdateRequest) -> String {
    match req.payload {
        Some(group_update_request::Payload::InviteMember(im)) => {
            format_wallet_address(&im.identity)
        }
        Some(group_update_request::Payload::RemoveMember(rm)) => {
            format_wallet_address(&rm.identity)
        }
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => ec
            .evidence
            .as_ref()
            .map(|e| format_wallet_address(&e.target_member_id))
            .unwrap_or_else(|| "unknown".to_string()),
        _ => "unknown".to_string(),
    }
}
