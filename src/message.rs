//! This module contains the messages that are used to communicate between inside the application
//! The high level message is a [`WakuMessage`](waku_bindings::WakuMessage)
//! Inside the [`WakuMessage`](waku_bindings::WakuMessage) we have a [`ContentTopic`](waku_bindings::WakuContentTopic) and a payload
//! The [`ContentTopic`](waku_bindings::WakuContentTopic) is used to identify the type of message and the payload is the actual message
//! Based on the [`ContentTopic`](waku_bindings::WakuContentTopic) we distinguish between:
//!  - [`WelcomeMessage`] which includes next message types:
//!    - [`GroupAnnouncement`]
//!         - `GroupAnnouncement {
//!             eth_pub_key: Vec<u8>,
//!             signature: Vec<u8>,
//!           }`
//!    - [`UserKeyPackage`]
//!         - `Encrypted KeyPackage: Vec<u8>`
//!    - [`InvitationToJoin`]
//!         - `Serialized MlsMessageOut: Vec<u8>`
//!  - [`AppMessage`]
//!    - [`ConversationMessage`]
//!    - [`BatchProposalsMessage`]
//!    - [`BanRequest`]
//!    - [`VotePayload`]
//!    - [`UserVote`]
//!
use alloy::hex;
use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    types::ConsensusEvent,
};
use mls_crypto::identity::normalize_wallet_address;
use openmls::prelude::{KeyPackage, MlsMessageOut};
use std::convert::TryFrom;

use crate::{
    encrypt_message,
    protos::de_mls::messages::v1::{
        app_message, welcome_message, AppMessage, BanRequest, BatchProposalsMessage,
        ConversationMessage, GroupAnnouncement, InvitationToJoin, Outcome, ProposalAdded,
        RequestType, UpdateRequest, UserKeyPackage, UserVote, VotePayload, WelcomeMessage,
    },
    steward::GroupUpdateRequest,
    verify_message, MessageError,
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

// WELCOME MESSAGE SUBTOPIC
impl GroupAnnouncement {
    pub fn new(pub_key: Vec<u8>, signature: Vec<u8>) -> Self {
        GroupAnnouncement {
            eth_pub_key: pub_key,
            signature,
        }
    }

    pub fn verify(&self) -> Result<bool, MessageError> {
        let verified = verify_message(&self.eth_pub_key, &self.signature, &self.eth_pub_key)?;
        Ok(verified)
    }

    pub fn encrypt(&self, kp: KeyPackage) -> Result<Vec<u8>, MessageError> {
        let key_package = serde_json::to_vec(&kp)?;
        let encrypted = encrypt_message(&key_package, &self.eth_pub_key)?;
        Ok(encrypted)
    }
}

impl From<GroupAnnouncement> for WelcomeMessage {
    fn from(group_announcement: GroupAnnouncement) -> Self {
        WelcomeMessage {
            payload: Some(welcome_message::Payload::GroupAnnouncement(
                group_announcement,
            )),
        }
    }
}

impl TryFrom<MlsMessageOut> for WelcomeMessage {
    type Error = MessageError;
    fn try_from(mls_message: MlsMessageOut) -> Result<Self, MessageError> {
        let mls_bytes = mls_message.to_bytes()?;
        let invitation = InvitationToJoin {
            mls_message_out_bytes: mls_bytes,
        };

        Ok(WelcomeMessage {
            payload: Some(welcome_message::Payload::InvitationToJoin(invitation)),
        })
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

impl From<GroupUpdateRequest> for UpdateRequest {
    fn from(group_update_request: GroupUpdateRequest) -> Self {
        match group_update_request {
            GroupUpdateRequest::AddMember(kp) => UpdateRequest {
                request_type: RequestType::AddMember as i32,
                wallet_address: kp.leaf_node().credential().serialized_content().to_vec(),
            },
            GroupUpdateRequest::RemoveMember(id) => UpdateRequest {
                request_type: RequestType::RemoveMember as i32,
                wallet_address: hex::decode(id.strip_prefix("0x").unwrap_or(&id))
                    .unwrap_or_else(|_| id.into_bytes()),
            },
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
