//! Core types for group operations.

use alloy::primitives::Address;
use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    types::ConsensusEvent,
};
use mls_crypto::{identity::normalize_wallet_address, KeyPackageBytes};
use std::{fmt::Display, str::FromStr};

use crate::protos::de_mls::messages::v1::{
    app_message, welcome_message, AppMessage, BanRequest, BatchProposalsMessage,
    ConversationMessage, InvitationToJoin, Outcome, ProposalAdded, RequestType, UpdateRequest,
    UserKeyPackage, UserVote, VotePayload, WelcomeMessage,
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

impl MessageType for UpdateRequest {
    fn message_type(&self) -> &'static str {
        match RequestType::try_from(self.request_type) {
            Ok(RequestType::AddMember) => "Add Member",
            Ok(RequestType::RemoveMember) => "Remove Member",
            _ => "Unknown",
        }
    }
}

/// Represents a group update request (add or remove member).
#[derive(Clone, Debug, PartialEq)]
pub enum GroupUpdateRequest {
    /// Add a member using their key package.
    AddMember(KeyPackageBytes),
    /// Remove a member by their identity (wallet address).
    RemoveMember(String),
}

impl Display for GroupUpdateRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupUpdateRequest::AddMember(kp) => {
                let id = Address::from_slice(kp.identity_bytes());
                write!(f, "Add Member: {id:#?}")
            }
            GroupUpdateRequest::RemoveMember(id) => {
                let id = Address::from_str(id).unwrap_or_default();
                write!(f, "Remove Member: {id:#?}")
            }
        }
    }
}

impl From<GroupUpdateRequest> for UpdateRequest {
    fn from(request: GroupUpdateRequest) -> Self {
        match request {
            GroupUpdateRequest::AddMember(kp) => UpdateRequest {
                request_type: RequestType::AddMember as i32,
                wallet_address: kp.identity_bytes().to_vec(),
            },
            GroupUpdateRequest::RemoveMember(identity) => {
                let wallet_bytes =
                    alloy::hex::decode(identity.strip_prefix("0x").unwrap_or(&identity))
                        .unwrap_or_default();
                UpdateRequest {
                    request_type: RequestType::RemoveMember as i32,
                    wallet_address: wallet_bytes,
                }
            }
        }
    }
}

/// Result of processing an inbound packet.
#[derive(Debug, Clone)]
pub enum ProcessResult {
    /// An application message was received.
    AppMessage(AppMessage),
    /// A consensus proposal was received.
    Proposal(hashgraph_like_consensus::protos::consensus::v1::Proposal),
    /// A consensus vote was received.
    Vote(hashgraph_like_consensus::protos::consensus::v1::Vote),
    /// The user should leave the group.
    LeaveGroup,
    /// A member proposal was added (for stewards receiving key packages).
    MemberProposalAdded(UpdateRequest),
    /// The user joined the group successfully.
    JoinedGroup(String),
    /// No action needed.
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

// Helper function to convert protobuf UpdateRequest to display format
pub fn convert_group_requests_to_display(
    group_requests: &[UpdateRequest],
) -> Vec<(String, String)> {
    let mut results = Vec::new();

    for req in group_requests {
        match RequestType::try_from(req.request_type) {
            Ok(RequestType::AddMember) => {
                results.push((
                    "Add Member".to_string(),
                    normalize_wallet_address(&req.wallet_address),
                ));
            }
            Ok(RequestType::RemoveMember) => {
                results.push((
                    "Remove Member".to_string(),
                    normalize_wallet_address(&req.wallet_address),
                ));
            }
            _ => {
                results.push(("Unknown".to_string(), "Invalid request".to_string()));
            }
        }
    }

    results
}
