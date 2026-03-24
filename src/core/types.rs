//! Core types for group operations.
//!
//! This module defines the key data types used throughout the DE-MLS core:
//!
//! - [`ProcessResult`] - Outcome of processing an inbound message
//! - Various `From` implementations for protobuf message conversions

use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    types::ConsensusEvent,
};

use crate::{
    core::CoreError,
    mls_crypto::parse_wallet_to_bytes,
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, CommitCandidate, ConversationMessage, EmergencyCriteriaProposal,
        GroupUpdateRequest, InvitationToJoin, Outcome, ProposalAdded, RemoveMember, UserKeyPackage,
        UserVote, ViolationEvidence, VotePayload, WelcomeMessage, app_message,
        group_update_request, welcome_message,
    },
};

/// Result of processing an inbound packet.
///
/// This enum represents all possible outcomes from [`process_inbound`](super::process_inbound).
/// Match it directly in your application layer to handle each variant.
///
/// # Variants
///
/// - `AppMessage` - A chat message or other application-level message
/// - `Proposal` / `Vote` - Consensus messages that need forwarding
/// - `GetUpdateRequest` - Steward received a membership change request
/// - `JoinedGroup` - Successfully joined via welcome message
/// - `GroupUpdated` - MLS state changed (batch commit applied)
/// - `LeaveGroup` - User was removed from the group
/// - `ViolationDetected` - Steward violation detected during commit validation
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
    /// `crate::app::forward_incoming_proposal`.
    Proposal(Proposal),

    /// A consensus vote was received from another peer.
    ///
    /// Should be forwarded to the consensus service via
    /// `crate::app::forward_incoming_vote`.
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
    /// Application should transition state back to Working.
    GroupUpdated,

    /// A steward violation was detected during commit validation.
    ///
    /// Contains evidence of the violation. The application should start
    /// an emergency criteria proposal vote for this evidence.
    ViolationDetected(ViolationEvidence),

    /// A remote commit candidate was successfully buffered in the freeze round.
    CandidateBuffered,

    /// No action needed.
    ///
    /// The message was not for us, was a duplicate, or required no action.
    Noop,
}

// ── ViolationEvidence constructors ────────────────────────────────

use crate::protos::de_mls::messages::v1::ViolationType;

impl ViolationEvidence {
    /// Steward included different proposal IDs than what was voted on,
    /// or IDs match but content digest differs.
    pub fn broken_commit(target: Vec<u8>, epoch: u64, payload: impl Into<Vec<u8>>) -> Self {
        Self {
            violation_type: ViolationType::BrokenCommit as i32,
            target_member_id: target,
            evidence_payload: payload.into(),
            epoch,
            creator_member_id: Vec::new(),
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
            creator_member_id: Vec::new(),
        }
    }

    /// Steward didn't commit within the threshold duration.
    pub fn censorship_inactivity(target: Vec<u8>, epoch: u64) -> Self {
        Self {
            violation_type: ViolationType::CensorshipInactivity as i32,
            target_member_id: target,
            evidence_payload: Vec::new(),
            epoch,
            creator_member_id: Vec::new(),
        }
    }

    /// Member's peer score dropped to or below the removal threshold.
    pub fn score_below_threshold(target: Vec<u8>, epoch: u64, current_score: i64) -> Self {
        Self {
            violation_type: ViolationType::ScoreBelowThreshold as i32,
            target_member_id: target,
            evidence_payload: current_score.to_le_bytes().to_vec(),
            epoch,
            creator_member_id: Vec::new(),
        }
    }

    /// Set the creator identity on this evidence (called by app layer before voting).
    pub fn with_creator(mut self, creator: Vec<u8>) -> Self {
        self.creator_member_id = creator;
        self
    }

    /// Wrap this evidence into a `GroupUpdateRequest` for consensus voting.
    ///
    /// Returns an error if `creator_member_id` is empty. Call `.with_creator()` before this
    /// method — every ECP must carry the creator identity for peer scoring (RFC §"Peer Scoring").
    pub fn into_update_request(self) -> Result<GroupUpdateRequest, CoreError> {
        if self.creator_member_id.is_empty() {
            return Err(CoreError::InvalidGroupUpdateRequest);
        }
        Ok(GroupUpdateRequest {
            payload: Some(group_update_request::Payload::EmergencyCriteria(
                EmergencyCriteriaProposal {
                    evidence: Some(self),
                },
            )),
        })
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

impl From<CommitCandidate> for AppMessage {
    fn from(commit_candidate: CommitCandidate) -> Self {
        AppMessage {
            payload: Some(app_message::Payload::CommitCandidate(commit_candidate)),
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
