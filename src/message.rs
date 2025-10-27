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
//!    - [`VotingProposal`]
//!    - [`UserVote`]
//!
use alloy::hex;
use openmls::prelude::{KeyPackage, MlsMessageOut};
use std::fmt::Display;

use crate::{
    consensus::ConsensusEvent,
    encrypt_message,
    protos::{
        consensus::v1::{
            ui_update_request, Outcome, Proposal, UiAddMemberRequest, UiRemoveMemberRequest,
            UiUpdateRequest, Vote,
        },
        de_mls::messages::v1::{
            app_message, welcome_message, AppMessage, BanRequest, BatchProposalsMessage,
            ClearCurrentEpochProposals, ConversationMessage, GroupAnnouncement, InvitationToJoin,
            ProposalAdded, UserKeyPackage, UserVote, VotingProposal, WelcomeMessage,
        },
    },
    steward::GroupUpdateRequest,
    verify_message, MessageError,
};

fn normalize_wallet_address(raw: &[u8]) -> String {
    let as_utf8 = std::str::from_utf8(raw)
        .map(|s| s.trim())
        .unwrap_or_default();

    if is_prefixed_hex(as_utf8) {
        return as_utf8.to_string();
    }

    if is_raw_hex(as_utf8) {
        return format!("0x{}", as_utf8);
    }

    if raw.is_empty() {
        String::new()
    } else {
        format!("0x{}", hex::encode(raw))
    }
}

fn is_prefixed_hex(input: &str) -> bool {
    let rest = input
        .strip_prefix("0x")
        .or_else(|| input.strip_prefix("0X"));
    match rest {
        Some(hex_part) if !hex_part.is_empty() => hex_part.chars().all(|c| c.is_ascii_hexdigit()),
        _ => false,
    }
}

fn is_raw_hex(input: &str) -> bool {
    !input.is_empty() && input.chars().all(|c| c.is_ascii_hexdigit())
}

// Message type constants for consistency and type safety
pub mod message_types {
    pub const CONVERSATION_MESSAGE: &str = "ConversationMessage";
    pub const BATCH_PROPOSALS_MESSAGE: &str = "BatchProposalsMessage";
    pub const BAN_REQUEST: &str = "BanRequest";
    pub const PROPOSAL: &str = "Proposal";
    pub const VOTE: &str = "Vote";
    pub const VOTING_PROPOSAL: &str = "VotingProposal";
    pub const USER_VOTE: &str = "UserVote";
    pub const PROPOSAL_ADDED: &str = "ProposalAdded";
    pub const CLEAR_CURRENT_EPOCH_PROPOSALS: &str = "ClearCurrentEpochProposals";
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
            app_message::Payload::VotingProposal(_) => VOTING_PROPOSAL,
            app_message::Payload::UserVote(_) => USER_VOTE,
            app_message::Payload::ProposalAdded(_) => PROPOSAL_ADDED,
            app_message::Payload::ClearCurrentEpochProposals(_) => CLEAR_CURRENT_EPOCH_PROPOSALS,
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

// APP MESSAGE SUBTOPIC
impl Display for AppMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.payload {
            Some(app_message::Payload::ConversationMessage(conversation_message)) => {
                write!(
                    f,
                    "{}: {}",
                    conversation_message.sender,
                    String::from_utf8_lossy(&conversation_message.message)
                )
            }
            Some(app_message::Payload::BatchProposalsMessage(batch_msg)) => {
                write!(
                    f,
                    "BatchProposalsMessage: {} proposals for group {}",
                    batch_msg.mls_proposals.len(),
                    String::from_utf8_lossy(&batch_msg.group_name)
                )
            }
            Some(app_message::Payload::BanRequest(ban_request)) => {
                write!(
                    f,
                    "SYSTEM: {} wants to ban {}",
                    ban_request.requester, ban_request.user_to_ban
                )
            }
            Some(app_message::Payload::Proposal(proposal)) => {
                write!(
                    f,
                    "Proposal: ID {} with {} votes for {} voters",
                    proposal.proposal_id,
                    proposal.votes.len(),
                    proposal.expected_voters_count
                )
            }
            Some(app_message::Payload::Vote(vote)) => {
                write!(
                    f,
                    "Vote: {} for proposal {} ({})",
                    if vote.vote { "YES" } else { "NO" },
                    vote.proposal_id,
                    vote.vote_id
                )
            }
            Some(app_message::Payload::VotingProposal(voting_proposal)) => {
                write!(
                    f,
                    "VotingProposal: ID {} for group {}",
                    voting_proposal.proposal_id, voting_proposal.group_name
                )
            }
            Some(app_message::Payload::UserVote(user_vote)) => {
                write!(
                    f,
                    "UserVote: {} for proposal {} in group {}",
                    if user_vote.vote { "YES" } else { "NO" },
                    user_vote.proposal_id,
                    user_vote.group_name
                )
            }
            Some(app_message::Payload::ProposalAdded(proposal_added)) => {
                write!(
                    f,
                    "ProposalAdded: {} {} in group {}",
                    proposal_added.action, proposal_added.address, proposal_added.group_id
                )
            }
            Some(app_message::Payload::ClearCurrentEpochProposals(clear_proposals)) => {
                write!(
                    f,
                    "ClearCurrentEpochProposals: clearing proposals for group {}",
                    clear_proposals.group_id
                )
            }
            None => write!(f, "Empty message"),
        }
    }
}

impl From<VotingProposal> for AppMessage {
    fn from(voting_proposal: VotingProposal) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::VotingProposal(voting_proposal)),
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

impl From<ClearCurrentEpochProposals> for AppMessage {
    fn from(clear_proposals: ClearCurrentEpochProposals) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::ClearCurrentEpochProposals(
                clear_proposals,
            )),
        }
    }
}

impl From<ConsensusEvent> for Outcome {
    fn from(consensus_event: ConsensusEvent) -> Self {
        match consensus_event {
            ConsensusEvent::ConsensusReached {
                proposal_id: _,
                result: true,
            } => Outcome::Accepted,
            ConsensusEvent::ConsensusReached {
                proposal_id: _,
                result: false,
            } => Outcome::Rejected,
            ConsensusEvent::ConsensusFailed {
                proposal_id: _,
                reason: _,
            } => Outcome::Unspecified,
        }
    }
}

impl From<GroupUpdateRequest> for UiUpdateRequest {
    fn from(group_update_request: GroupUpdateRequest) -> Self {
        match group_update_request {
            GroupUpdateRequest::AddMember(kp) => UiUpdateRequest {
                request: Some(ui_update_request::Request::AddMember(UiAddMemberRequest {
                    wallet_address: kp.leaf_node().credential().serialized_content().to_vec(),
                })),
            },
            GroupUpdateRequest::RemoveMember(id) => UiUpdateRequest {
                request: Some(ui_update_request::Request::RemoveMember(
                    UiRemoveMemberRequest {
                        wallet_address: id.into(),
                    },
                )),
            },
        }
    }
}

// Helper function to convert protobuf UiUpdateRequest to display format
pub fn convert_group_requests_to_display(
    group_requests: &[UiUpdateRequest],
) -> Vec<(String, String)> {
    let mut results = Vec::new();

    for req in group_requests {
        match &req.request {
            Some(ui_update_request::Request::AddMember(add_req)) => {
                results.push((
                    "Add Member".to_string(),
                    normalize_wallet_address(&add_req.wallet_address),
                ));
            }
            Some(ui_update_request::Request::RemoveMember(remove_req)) => {
                results.push((
                    "Remove Member".to_string(),
                    normalize_wallet_address(&remove_req.wallet_address),
                ));
            }
            None => {
                results.push(("Unknown".to_string(), "Invalid request".to_string()));
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::{is_prefixed_hex, normalize_wallet_address};

    #[test]
    fn keeps_prefixed_hex() {
        let addr = normalize_wallet_address(b"0xAbCd1234");
        assert_eq!(addr, "0xAbCd1234");
    }

    #[test]
    fn prefixes_raw_hex() {
        let addr = normalize_wallet_address(b"ABCD1234");
        assert_eq!(addr, "0xABCD1234");
    }

    #[test]
    fn encodes_binary_bytes() {
        let addr = normalize_wallet_address(&[0x11, 0x22, 0x33]);
        assert_eq!(addr, "0x112233");
    }

    #[test]
    fn trims_ascii_input() {
        let addr = normalize_wallet_address(b"  0x1F  ");
        assert_eq!(addr, "0x1F");
    }

    #[test]
    fn prefixed_hex_helper() {
        assert!(is_prefixed_hex("0xabc"));
        assert!(is_prefixed_hex("0XABC"));
        assert!(!is_prefixed_hex("abc"));
        assert!(!is_prefixed_hex("0x"));
    }
}
