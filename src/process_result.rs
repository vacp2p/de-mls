use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    types::ConsensusEvent,
};

use crate::{
    ConversationError, ScoreEvent, ScoreOp,
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, CommitCandidate, ConversationMessage, ConversationSync,
        ConversationUpdateRequest, EmergencyCriteriaProposal, EventMembershipChange, MemberWelcome,
        Outcome, ProposalAdded, UserVote, ViolationEvidence, ViolationType, VotePayload,
        app_message, conversation_update_request,
    },
};

/// Outcome of processing one inbound packet. The application layer should matches this
/// directly and dispatches the side effects.
#[derive(Debug, Clone)]
pub enum ProcessResult {
    /// Decrypted application message ready to deliver to the UI.
    AppMessage(Box<AppMessage>),

    /// Consensus proposal from a peer — forward to the consensus service.
    Proposal(Box<Proposal>),

    /// Consensus vote from a peer — forward to the consensus service.
    Vote(Box<Vote>),

    /// We were removed from the conversation.
    LeaveConversation,

    /// Steward received a membership change (invite KP / ban) — start a vote.
    MembershipChangeReceived(Box<ConversationUpdateRequest>),

    /// MLS state advanced (batch commit applied).
    ConversationUpdated,

    /// Remote commit candidate was buffered in the active freeze round.
    CommitCandidateReceived { steward_id: Vec<u8> },

    /// Conversation-sync message from the steward.
    ConversationSyncReceived(Box<ConversationSync>),

    /// Welcome broadcast from the committing steward: every member learns
    /// the welcome so the application decides who delivers it to the
    /// joiners and how.
    WelcomeBroadcastReceived(Box<MemberWelcome>),

    /// Nothing to do.
    Noop(NoopReason),
}

/// Why a [`ProcessResult::Noop`] was returned. One variant per producer
/// site so the dispatch layer can match on the specific case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoopReason {
    /// Decrypted application message had no recognized payload variant.
    UnknownAppMessage,
    /// Fast-path proposal rejected: MLS sender doesn't match the
    /// self-removal target.
    FastPathRejected,
    /// Ban request dropped: target is not a conversation member.
    BanTargetNotMember,
    /// Decrypt returned `Ignored` (wrong epoch or wrong conversation).
    DecryptIgnored,
    /// Decrypt returned a non-Application MLS payload on the app subtopic.
    UnexpectedMlsType,
    /// No approved proposals to commit.
    NoApprovedProposals,
    /// Commit hash matches a recent committed batch — duplicate broadcast.
    AlreadyCommitted,
    /// Candidate carried an empty proposals or commit payload.
    EmptyCandidatePayload,
    /// Candidate carried an empty `steward_member_id` field.
    EmptyStewardMemberId,
    /// Candidate's wire kind doesn't match Proposal/Commit.
    WireKindMismatch,
    /// Identical commit hash is already buffered for this round.
    DuplicateBufferedHash,
    /// Freeze-round buffer is full (one candidate per member already held).
    CandidateBufferFull,
    /// Candidate arrived before its proposal was locally approved (consensus
    /// outcome still in flight). Stashed for replay once approval lands.
    CandidateStashedEarly,
    /// Welcome broadcast carried no welcome bytes.
    EmptyWelcomePayload,
    /// Welcome broadcast hash was already seen — duplicate gossip delivery.
    DuplicateWelcomeBroadcast,
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

    /// Layer 3 anti-deadlock signal — on YES the steward gate relaxes so
    /// any member can produce the recovery commit. No specific target.
    pub fn deadlock(epoch: u64) -> Self {
        Self {
            violation_type: ViolationType::Deadlock as i32,
            target_member_id: Vec::new(),
            evidence_payload: Vec::new(),
            epoch,
            creator_member_id: Vec::new(),
        }
    }

    /// Set the creator member_id on this evidence (called by app layer before voting).
    pub fn with_creator(mut self, creator: Vec<u8>) -> Self {
        self.creator_member_id = creator;
        self
    }

    /// Wrap this evidence into a `ConversationUpdateRequest` for consensus voting.
    ///
    /// Returns an error if `creator_member_id` is empty. Call `.with_creator()` before this
    /// method — every ECP must carry the creator member_id for peer scoring (RFC §"Peer Scoring").
    pub fn into_update_request(self) -> Result<ConversationUpdateRequest, ConversationError> {
        if self.creator_member_id.is_empty() {
            return Err(ConversationError::InvalidConversationUpdateRequest);
        }
        Ok(ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::EmergencyCriteria(
                EmergencyCriteriaProposal {
                    evidence: Some(self),
                },
            )),
        })
    }

    /// Peer-score penalty the target takes for this violation, or `None`
    /// for violation types that have no target-side score (`ScoreBelowThreshold`
    /// drives a removal, not a penalty; `Deadlock` has no target;
    /// `Unspecified` and unknown wire values are malformed).
    pub fn target_score_event(&self) -> Option<ScoreEvent> {
        match ViolationType::try_from(self.violation_type) {
            Ok(ViolationType::BrokenCommit) => Some(ScoreEvent::BrokenCommit),
            Ok(ViolationType::BrokenMlsProposal) => Some(ScoreEvent::BrokenMlsProposal),
            Ok(ViolationType::CensorshipInactivity) => Some(ScoreEvent::CensorshipInactivity),
            Ok(ViolationType::ScoreBelowThreshold)
            | Ok(ViolationType::Deadlock)
            | Ok(ViolationType::ViolationUnspecified)
            | Err(_) => None,
        }
    }

    /// `ScoreOp` applying [`Self::target_score_event`] to [`Self::target_member_id`].
    /// `None` when the violation type carries no target-side score.
    pub fn target_score_op(&self) -> Option<ScoreOp> {
        Some(ScoreOp {
            member_id: self.target_member_id.clone(),
            event: self.target_score_event()?,
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
    AppMessage,
    VotePayload         => app_message::Payload::VotePayload,
    UserVote            => app_message::Payload::UserVote,
    ConversationMessage => app_message::Payload::ConversationMessage,
    CommitCandidate     => app_message::Payload::CommitCandidate,
    BanRequest          => app_message::Payload::BanRequest,
    Proposal            => app_message::Payload::Proposal,
    Vote                => app_message::Payload::Vote,
    ConversationSync    => app_message::Payload::ConversationSync,
    ProposalAdded       => app_message::Payload::ProposalAdded,
    MemberWelcome       => app_message::Payload::MemberWelcome,
    EventMembershipChange => app_message::Payload::MembershipChange,
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
    type Error = ConversationError;
    fn try_from(value: AppMessage) -> Result<Self, Self::Error> {
        match &value.payload {
            Some(app_message::Payload::ConversationMessage(_)) => {
                Ok(ProcessResult::AppMessage(Box::new(value)))
            }
            Some(app_message::Payload::MembershipChange(_)) => {
                Ok(ProcessResult::AppMessage(Box::new(value)))
            }
            Some(app_message::Payload::Proposal(proposal)) => {
                Ok(ProcessResult::Proposal(Box::new(proposal.clone())))
            }
            Some(app_message::Payload::Vote(vote)) => {
                Ok(ProcessResult::Vote(Box::new(vote.clone())))
            }
            Some(app_message::Payload::BanRequest(ban_request)) => {
                Ok(ProcessResult::MembershipChangeReceived(Box::new(
                    ConversationUpdateRequest::remove_member(ban_request.user_to_ban.clone()),
                )))
            }
            Some(app_message::Payload::ConversationSync(sync)) => Ok(
                ProcessResult::ConversationSyncReceived(Box::new(sync.clone())),
            ),
            other => {
                tracing::debug!(
                    payload_kind = ?other.as_ref().map(std::mem::discriminant),
                    "app message ignored: payload variant not consumed by core dispatch"
                );
                Ok(ProcessResult::Noop(NoopReason::UnknownAppMessage))
            }
        }
    }
}
