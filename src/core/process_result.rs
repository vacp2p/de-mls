//! [`ProcessResult`] returned by [`process_inbound`](super::process_inbound)
//! plus protobuf ↔ message-envelope `From` impls.

use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    types::ConsensusEvent,
};

use crate::{
    core::CoreError,
    mls_crypto::parse_wallet_to_bytes,
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, CommitCandidate, ConversationMessage, EmergencyCriteriaProposal,
        GroupSync, GroupUpdateRequest, InvitationToJoin, LeaveRequest, Outcome, ProposalAdded,
        RemoveMember, UserKeyPackage, UserVote, ViolationEvidence, ViolationType, VotePayload,
        WelcomeMessage, app_message, group_update_request, welcome_message,
    },
};

/// Outcome of processing one inbound packet. The app layer matches this
/// directly and dispatches the side effects.
#[derive(Debug, Clone)]
pub enum ProcessResult {
    /// Decrypted application message ready to deliver to the UI.
    AppMessage(AppMessage),

    /// Consensus proposal from a peer — forward to the consensus service.
    Proposal(Proposal),

    /// Consensus vote from a peer — forward to the consensus service.
    Vote(Vote),

    /// We were removed from the group.
    LeaveGroup,

    /// Steward received a membership change (invite KP / ban) — start a vote.
    MembershipChangeReceived(GroupUpdateRequest),

    /// Successfully joined via a welcome message; carries the group name.
    JoinedGroup(String),

    /// MLS state advanced (batch commit applied).
    GroupUpdated,

    /// Commit-validation caught a steward violation — file an emergency
    /// criteria proposal with this evidence.
    ViolationDetected(ViolationEvidence),

    /// Remote commit candidate was buffered in the active freeze round.
    CommitCandidateReceived,

    /// Group-sync message from the steward (steward list, scores, timing,
    /// protocol flags). Meaningful only for joiners with no steward list yet.
    GroupSyncReceived(GroupSync),

    /// Authenticated self-leave from a member. Auto-approved — the app layer
    /// verifies the signer matches `identity` and inserts `RemoveMember`
    /// directly into the approved queue.
    LeaveRequestReceived(LeaveRequest),

    /// Nothing to do (not for us, duplicate, or already handled).
    Noop,
}

// ── ViolationEvidence constructors ────────────────────────────────

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

/// Build `impl From<Inner> for Envelope` where
/// `Envelope { payload: Some(<variant path>(Inner)) }`.
macro_rules! impl_payload_from {
    ($envelope:ty, $( $inner:ty => $variant:path ),+ $(,)?) => {
        $(
            impl From<$inner> for $envelope {
                fn from(value: $inner) -> Self {
                    Self { payload: Some($variant(value)) }
                }
            }
        )+
    };
}

impl_payload_from!(
    WelcomeMessage,
    UserKeyPackage   => welcome_message::Payload::UserKeyPackage,
    InvitationToJoin => welcome_message::Payload::InvitationToJoin,
);

impl_payload_from!(
    AppMessage,
    VotePayload         => app_message::Payload::VotePayload,
    UserVote            => app_message::Payload::UserVote,
    ConversationMessage => app_message::Payload::ConversationMessage,
    CommitCandidate     => app_message::Payload::CommitCandidate,
    BanRequest          => app_message::Payload::BanRequest,
    LeaveRequest        => app_message::Payload::LeaveRequest,
    Proposal            => app_message::Payload::Proposal,
    Vote                => app_message::Payload::Vote,
    GroupSync           => app_message::Payload::GroupSync,
    ProposalAdded       => app_message::Payload::ProposalAdded,
);

impl From<ConsensusEvent> for Outcome {
    fn from(ev: ConsensusEvent) -> Self {
        match ev {
            ConsensusEvent::ConsensusReached { result: true, .. } => Outcome::Accepted,
            ConsensusEvent::ConsensusReached { result: false, .. } => Outcome::Rejected,
            ConsensusEvent::ConsensusFailed { .. } => Outcome::Unspecified,
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
            Some(app_message::Payload::BanRequest(ban_request)) => Ok(
                ProcessResult::MembershipChangeReceived(GroupUpdateRequest {
                    payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                        identity: parse_wallet_to_bytes(ban_request.user_to_ban.as_str())?,
                    })),
                }),
            ),
            Some(app_message::Payload::GroupSync(sync)) => {
                Ok(ProcessResult::GroupSyncReceived(sync.clone()))
            }
            // LeaveRequest is NOT handled here: the authenticated path
            // (`process_app_subtopic`) is the sole producer of
            // `LeaveRequestReceived` — it verifies the MLS sender matches
            // `leave.identity` before emitting the variant. Treating it as
            // Noop here prevents any caller from bypassing that check via
            // a generic `AppMessage::try_into()`.
            _ => Ok(ProcessResult::Noop),
        }
    }
}
