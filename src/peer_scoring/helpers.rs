//! Pure helpers: scoring/member-set diff and ECP score-op derivation.

use std::collections::HashSet;

use crate::{
    Verdict,
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, ViolationEvidence, conversation_update_request::Payload,
    },
    {ScoreEvent, ScoreOp, ScoringMemberDiff},
};

/// Diff between a scoring table snapshot and an MLS member set.
/// Caller applies the diff to its own [`crate::PeerScoringService`].
pub fn scoring_member_diff(scored: &[Vec<u8>], mls_members: &[Vec<u8>]) -> ScoringMemberDiff {
    let scored_set: HashSet<&[u8]> = scored.iter().map(Vec::as_slice).collect();
    let mls_set: HashSet<&[u8]> = mls_members.iter().map(Vec::as_slice).collect();

    let to_add = mls_members
        .iter()
        .filter(|m| !scored_set.contains(m.as_slice()))
        .cloned()
        .collect();
    let to_remove = scored
        .iter()
        .filter(|m| !mls_set.contains(m.as_slice()))
        .cloned()
        .collect();
    ScoringMemberDiff { to_add, to_remove }
}

/// Converts a finished emergency-proposal vote into the peer-score changes
/// it implies. The proposal accuses a target of a violation; once the group
/// has voted (`verdict` is the outcome), the creator and the target gain or
/// lose score depending on the outcome.
///
/// Returns an empty vector when `request` isn't an emergency
/// proposal or carries no evidence.
///
/// - approved, violation carries a target penalty → target penalty + creator reward.
/// - approved, no target penalty → creator reward only.
/// - rejected (false accusation) → creator penalty.
/// - failed (no decision) → nothing; nobody is blamed for a tie.
pub fn emergency_score_ops(request: &ConversationUpdateRequest, verdict: Verdict) -> Vec<ScoreOp> {
    let Some(Payload::EmergencyCriteria(ec)) = &request.payload else {
        return Vec::new();
    };
    let Some(evidence) = &ec.evidence else {
        return Vec::new();
    };
    match verdict {
        Verdict::Failed => Vec::new(),
        Verdict::Rejected => vec![creator_penalty(evidence)],
        Verdict::Approved => {
            let mut ops = vec![creator_reward(evidence)];
            if let Some(target_op) = evidence.target_score_op() {
                ops.push(target_op);
            }
            ops
        }
    }
}

fn creator_reward(ev: &ViolationEvidence) -> ScoreOp {
    ScoreOp {
        member_id: ev.creator_member_id.clone(),
        event: ScoreEvent::EmergencyYesCreator,
    }
}

fn creator_penalty(ev: &ViolationEvidence) -> ScoreOp {
    ScoreOp {
        member_id: ev.creator_member_id.clone(),
        event: ScoreEvent::EmergencyNoCreator,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::de_mls::messages::v1::{EmergencyCriteriaProposal, ViolationType};

    fn ecp_request(
        violation_type: i32,
        target: Vec<u8>,
        creator: Vec<u8>,
    ) -> ConversationUpdateRequest {
        let evidence = ViolationEvidence {
            violation_type,
            target_member_id: target,
            evidence_payload: Vec::new(),
            epoch: 0,
            creator_member_id: creator,
        };
        ConversationUpdateRequest {
            payload: Some(Payload::EmergencyCriteria(EmergencyCriteriaProposal {
                evidence: Some(evidence),
            })),
        }
    }

    // ── scoring_member_diff ────────────────────────────────────────

    /// Members MLS knows about but scoring doesn't are additions; the reverse
    /// are removals. This is what keeps the score table tracking the member
    /// set, so both directions have to be picked up in one pass.
    #[test]
    fn diff_reports_additions_and_removals_together() {
        let diff = scoring_member_diff(
            &[b"alice".to_vec(), b"gone".to_vec()],
            &[b"alice".to_vec(), b"newcomer".to_vec()],
        );
        assert_eq!(diff.to_add, vec![b"newcomer".to_vec()]);
        assert_eq!(diff.to_remove, vec![b"gone".to_vec()]);
    }

    /// Tables already in sync must produce no work — otherwise every epoch
    /// would re-add and re-remove the same members, resetting their scores.
    #[test]
    fn diff_of_matching_sets_is_empty() {
        let members = vec![b"alice".to_vec(), b"bob".to_vec()];
        let diff = scoring_member_diff(&members, &members);
        assert!(diff.to_add.is_empty());
        assert!(diff.to_remove.is_empty());
    }

    /// An empty scoring table means everyone is new (first bootstrap), and an
    /// empty member set means everyone goes.
    #[test]
    fn diff_handles_empty_sides() {
        let members = vec![b"alice".to_vec()];
        let bootstrap = scoring_member_diff(&[], &members);
        assert_eq!(bootstrap.to_add, members);
        assert!(bootstrap.to_remove.is_empty());

        let emptied = scoring_member_diff(&members, &[]);
        assert!(emptied.to_add.is_empty());
        assert_eq!(emptied.to_remove, members);
    }

    // ── emergency_score_ops ────────────────────────────────────────

    /// Not every resolved proposal is an accusation. A membership change
    /// carries no evidence and must score nobody, whatever the verdict.
    #[test]
    fn non_emergency_request_scores_nobody() {
        use crate::protos::de_mls::messages::v1::RemoveMember;
        let req = ConversationUpdateRequest {
            payload: Some(Payload::RemoveMember(RemoveMember {
                member_id: vec![0xAA],
            })),
        };
        for verdict in [Verdict::Approved, Verdict::Rejected, Verdict::Failed] {
            assert!(emergency_score_ops(&req, verdict).is_empty());
        }
    }

    /// An ECP with no evidence names no creator to reward or penalize, so it
    /// must score nobody rather than emit an op against an empty member id.
    #[test]
    fn evidenceless_emergency_scores_nobody() {
        let req = ConversationUpdateRequest {
            payload: Some(Payload::EmergencyCriteria(EmergencyCriteriaProposal {
                evidence: None,
            })),
        };
        for verdict in [Verdict::Approved, Verdict::Rejected, Verdict::Failed] {
            assert!(emergency_score_ops(&req, verdict).is_empty());
        }
    }

    /// Approved + target-mappable violation → creator reward + target penalty.
    #[test]
    fn approved_broken_commit_emits_reward_and_target_penalty() {
        let req = ecp_request(ViolationType::BrokenCommit as i32, vec![0xAA], vec![0xBB]);
        let ops = emergency_score_ops(&req, Verdict::Approved);
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
        assert_eq!(ops[0].member_id, vec![0xBB]);
        assert_eq!(ops[1].event, ScoreEvent::BrokenCommit);
        assert_eq!(ops[1].member_id, vec![0xAA]);
    }

    /// Approved + non-target violation (`Deadlock`) → creator reward only.
    #[test]
    fn approved_deadlock_emits_reward_only() {
        let req = ecp_request(ViolationType::Deadlock as i32, Vec::new(), vec![0xBB]);
        let ops = emergency_score_ops(&req, Verdict::Approved);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Wire-malformed `Unspecified` violation type — emit creator reward,
    /// drop the malformed target op silently.
    #[test]
    fn approved_unspecified_emits_reward_only() {
        let req = ecp_request(0, vec![0xAA], vec![0xBB]);
        let ops = emergency_score_ops(&req, Verdict::Approved);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Rejected → creator penalty regardless of violation type.
    #[test]
    fn rejected_emits_creator_penalty() {
        for vt in [
            ViolationType::BrokenCommit,
            ViolationType::Deadlock,
            ViolationType::ScoreBelowThreshold,
        ] {
            let req = ecp_request(vt as i32, vec![0xAA], vec![0xBB]);
            let ops = emergency_score_ops(&req, Verdict::Rejected);
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].event, ScoreEvent::EmergencyNoCreator);
            assert_eq!(ops[0].member_id, vec![0xBB]);
        }
    }

    /// Failed (no decision at the timeout) → nobody is scored, whatever the
    /// violation type. A tie blames neither the creator nor the target.
    #[test]
    fn failed_emergency_scores_nobody() {
        for vt in [
            ViolationType::BrokenCommit,
            ViolationType::Deadlock,
            ViolationType::ScoreBelowThreshold,
        ] {
            let req = ecp_request(vt as i32, vec![0xAA], vec![0xBB]);
            assert!(emergency_score_ops(&req, Verdict::Failed).is_empty());
        }
    }
}
