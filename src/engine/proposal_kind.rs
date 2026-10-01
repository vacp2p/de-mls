//! Classification of [`ConversationUpdateRequest`]s: the protocol role of a
//! request, the change it makes to the group, and the member it targets.
//!
//! `ProposalKind` is the canonical classifier for membership-vs-governance
//! proposals. The ordinal order encodes RFC partial-freeze priority
//! (Commit < StewardElection < Emergency), used by
//! `EngineQueues::partial_freeze_blocks`.

use crate::{
    engine::types::ActionKind,
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, ViolationType, conversation_update_request::Payload,
    },
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

/// The change `req` makes and the member it keys on: an invite adds the
/// joiner's id as the proposer stamped it, a removal targets its member, a
/// leaf update belongs to its owner. `None` for governance payloads, which
/// never reach a commit.
pub(crate) fn change_of(req: &ConversationUpdateRequest) -> Option<(ActionKind, &[u8])> {
    match req.payload.as_ref()? {
        Payload::MemberInvite(m) => Some((ActionKind::Add, &m.member_id)),
        Payload::RemoveMember(m) => Some((ActionKind::Remove, &m.member_id)),
        Payload::LeafUpdate(m) => Some((ActionKind::Update, &m.member_id)),
        _ => None,
    }
}

/// The target of a membership-changing `ConversationUpdateRequest`: a removal's
/// `member_id`, or an invite's joiner id as the proposer stamped it. Both are
/// the same key space, so a joiner has one id before and after it is seated.
/// `None` for other payloads, or an invite with no id.
pub(crate) fn target_member_id_of(request: &ConversationUpdateRequest) -> Option<Vec<u8>> {
    match change_of(request)? {
        (ActionKind::Add, []) => None,
        (ActionKind::Add | ActionKind::Remove, id) => Some(id.to_vec()),
        (ActionKind::Update, _) => None,
    }
}

/// Member a proposal will add or remove once it lands — the membership target,
/// plus a score-below-threshold ECP, which transforms into a RemoveMember on
/// approval. Nothing in de-mls raises that ECP; an integrator acting on a low
/// score can. Covering it here lets the in-flight index dedup a repeated
/// proposal against one for its whole voting window, not just after it
/// becomes a RemoveMember.
pub(crate) fn in_flight_target(request: &ConversationUpdateRequest) -> Option<Vec<u8>> {
    if let Some(id) = target_member_id_of(request) {
        return Some(id);
    }
    let Payload::EmergencyCriteria(ec) = request.payload.as_ref()? else {
        return None;
    };
    let ev = ec.evidence.as_ref()?;
    (ViolationType::try_from(ev.violation_type) == Ok(ViolationType::ScoreBelowThreshold))
        .then(|| ev.target_member_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::de_mls::messages::v1::{
        EmergencyCriteriaProposal, LeafUpdate, MemberInvite, RemoveMember, StewardElectionProposal,
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
        assert_eq!(
            ProposalKind::of(&req(Payload::LeafUpdate(LeafUpdate::default()))),
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

    #[test]
    fn change_of_names_the_kind_and_member() {
        let id = vec![7u8];
        let invite = MemberInvite {
            member_id: id.clone(),
            ..Default::default()
        };
        let remove = RemoveMember {
            member_id: id.clone(),
        };
        let update = LeafUpdate {
            member_id: id.clone(),
            ..Default::default()
        };
        assert_eq!(
            change_of(&req(Payload::MemberInvite(invite))),
            Some((ActionKind::Add, id.as_slice()))
        );
        assert_eq!(
            change_of(&req(Payload::RemoveMember(remove))),
            Some((ActionKind::Remove, id.as_slice()))
        );
        assert_eq!(
            change_of(&req(Payload::LeafUpdate(update))),
            Some((ActionKind::Update, id.as_slice()))
        );
        let election = req(Payload::StewardElection(StewardElectionProposal::default()));
        assert_eq!(change_of(&election), None);
    }

    /// RFC partial-freeze priority: Emergency > StewardElection > Commit.
    /// `ConversationQueues::partial_freeze_blocks` and any future cross-kind
    /// priority check rely on this ordering.
    #[test]
    fn priority_ordering() {
        assert!(ProposalKind::Emergency > ProposalKind::StewardElection);
        assert!(ProposalKind::StewardElection > ProposalKind::Commit);
        assert!(ProposalKind::Emergency > ProposalKind::Commit);
    }
}
