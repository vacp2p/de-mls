//! Protobuf message definitions, plus constructors for the common
//! request shapes.

/// Re-exported consensus protocol messages.
pub mod hashgraph_like_consensus {
    pub mod v1 {
        pub use ::hashgraph_like_consensus::protos::consensus::v1::*;
    }
}

/// DE-MLS application-level messages.
pub mod de_mls {
    pub mod messages {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/de_mls.messages.v1.rs"));
        }
    }

    /// Engine state snapshots; local encoding, never sent.
    pub mod engine {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/de_mls.engine.v1.rs"));
        }
    }
}

use de_mls::messages::v1::{
    ConversationUpdateRequest, EmergencyCriteriaProposal, MemberInvite, RemoveMember,
    StewardElectionProposal, ViolationEvidence, ViolationType,
    conversation_update_request::Payload,
};

use crate::{ConversationError, ScoreEvent, ScoreOp};

impl ConversationUpdateRequest {
    /// `RemoveMember` request targeting `member_id`.
    pub fn remove_member(member_id: impl Into<Vec<u8>>) -> Self {
        Self {
            payload: Some(Payload::RemoveMember(RemoveMember {
                member_id: member_id.into(),
            })),
        }
    }

    /// `MemberInvite` request carrying a joiner's key package.
    pub fn member_invite(invite: MemberInvite) -> Self {
        Self {
            payload: Some(Payload::MemberInvite(invite)),
        }
    }

    /// `StewardElection` request proposing `election`.
    pub fn steward_election(election: StewardElectionProposal) -> Self {
        Self {
            payload: Some(Payload::StewardElection(election)),
        }
    }

    /// True iff `req` is a membership change that produces an MLS proposal
    pub fn produces_mls_action(&self) -> bool {
        matches!(
            self.payload.as_ref(),
            Some(Payload::MemberInvite(_) | Payload::RemoveMember(_))
        )
    }
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

    /// Layer 3 anti-deadlock signal — on YES the silent epoch steward is
    /// skipped for the epoch and the next eligible steward takes over. No
    /// specific target.
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
            payload: Some(Payload::EmergencyCriteria(EmergencyCriteriaProposal {
                evidence: Some(self),
            })),
        })
    }

    /// Peer-score penalty the target takes for this violation, or `None`
    /// for violation types that have no target-side score (`ScoreBelowThreshold`
    /// drives a removal, not a penalty; `Deadlock` has no target;
    /// `Unspecified` and unknown wire values are malformed).
    fn target_score_event(&self) -> Option<ScoreEvent> {
        match ViolationType::try_from(self.violation_type) {
            Ok(ViolationType::BrokenCommit) => Some(ScoreEvent::BrokenCommit),
            Ok(ViolationType::BrokenMlsProposal) => Some(ScoreEvent::BrokenMlsProposal),
            Ok(ViolationType::CensorshipInactivity) => Some(ScoreEvent::CensorshipInactivity),
            Ok(ViolationType::MisbehavingCommit) => Some(ScoreEvent::MisbehavingCommit),
            Ok(ViolationType::ScoreBelowThreshold)
            | Ok(ViolationType::Deadlock)
            | Ok(ViolationType::ViolationUnspecified)
            | Err(_) => None,
        }
    }

    /// `ScoreOp` applying [`Self::target_score_event`] to `target_member_id`.
    /// `None` when the violation type carries no target-side score.
    pub(crate) fn target_score_op(&self) -> Option<ScoreOp> {
        Some(ScoreOp {
            member_id: self.target_member_id.clone(),
            event: self.target_score_event()?,
        })
    }
}
