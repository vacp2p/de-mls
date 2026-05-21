//! Classification of [`ConversationUpdateRequest`]s by protocol role.
//!
//! `ProposalKind` is the canonical classifier for membership-vs-governance
//! proposals. The ordinal order encodes RFC partial-freeze priority
//! (Commit < StewardElection < Emergency), used by
//! [`Conversation::partial_freeze_blocks`](crate::core::Conversation::partial_freeze_blocks).

use crate::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, conversation_update_request::Payload,
};

/// Protocol role of a `ConversationUpdateRequest`. Ordinal order is RFC priority
/// (higher variant beats lower when both are in flight).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProposalKind {
    /// Membership change (invite / remove member). Lowest priority.
    Commit = 0,
    /// Steward rotation. Between commit and emergency.
    StewardElection = 1,
    /// Emergency criteria — highest priority; partially freezes lower kinds.
    Emergency = 2,
}

impl ProposalKind {
    /// Classify a `ConversationUpdateRequest`. `None` / unknown payloads map to [`Self::Commit`].
    pub fn of(req: &ConversationUpdateRequest) -> Self {
        match &req.payload {
            Some(Payload::EmergencyCriteria(_)) => Self::Emergency,
            Some(Payload::StewardElection(_)) => Self::StewardElection,
            _ => Self::Commit,
        }
    }

    pub fn is_emergency(self) -> bool {
        self == Self::Emergency
    }

    pub fn is_steward_election(self) -> bool {
        self == Self::StewardElection
    }

    /// True for kinds that produce an MLS proposal (invite / remove member).
    /// Emergency and election are consensus-only — they never land in an MLS commit.
    pub fn is_mls_producing(self) -> bool {
        self == Self::Commit
    }

    /// True for emergency or steward-election kinds (non-commit governance).
    pub fn is_governance(self) -> bool {
        !self.is_mls_producing()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::de_mls::messages::v1::{
        EmergencyCriteriaProposal, MemberInvite, RemoveMember, StewardElectionProposal,
        ViolationEvidence,
    };

    fn req(payload: Payload) -> ConversationUpdateRequest {
        ConversationUpdateRequest {
            payload: Some(payload),
        }
    }

    #[test]
    fn invite_and_remove_are_commit() {
        assert_eq!(
            ProposalKind::of(&req(Payload::MemberInvite(MemberInvite::default()))),
            ProposalKind::Commit,
        );
        assert_eq!(
            ProposalKind::of(&req(Payload::RemoveMember(RemoveMember::default()))),
            ProposalKind::Commit,
        );
    }

    #[test]
    fn emergency_classified() {
        let r = req(Payload::EmergencyCriteria(EmergencyCriteriaProposal {
            evidence: Some(ViolationEvidence::broken_commit(vec![], 0, vec![])),
        }));
        let k = ProposalKind::of(&r);
        assert_eq!(k, ProposalKind::Emergency);
        assert!(k.is_emergency());
        assert!(k.is_governance());
        assert!(!k.is_mls_producing());
    }

    #[test]
    fn steward_election_classified() {
        let r = req(Payload::StewardElection(StewardElectionProposal {
            proposed_stewards: vec![vec![1], vec![2]],
            election_epoch: 5,
            retry_round: 0,
        }));
        let k = ProposalKind::of(&r);
        assert_eq!(k, ProposalKind::StewardElection);
        assert!(k.is_steward_election());
        assert!(k.is_governance());
    }

    /// RFC partial-freeze priority: Emergency > StewardElection > Commit.
    /// [`Conversation::partial_freeze_blocks`] and any future cross-kind
    /// priority check rely on this ordering.
    #[test]
    fn priority_ordering() {
        assert!(ProposalKind::Emergency > ProposalKind::StewardElection);
        assert!(ProposalKind::StewardElection > ProposalKind::Commit);
        assert!(ProposalKind::Emergency > ProposalKind::Commit);
    }
}
