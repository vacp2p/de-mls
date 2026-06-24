//! Pure helpers: scoring/member-roster diff and ECP score-op derivation.

use std::collections::HashSet;

use crate::{
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, ViolationEvidence, conversation_update_request::Payload,
    },
    {ScoreEvent, ScoreOp, ScoringMemberDiff},
};

/// Diff between a scoring table snapshot and an MLS member roster.
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
/// has voted (`approved` is the result), the creator and the target gain or
/// lose score depending on the outcome.
///
/// Returns an empty vector when `request` isn't an emergency
/// proposal or carries no evidence.
///
/// - accepted, violation carries a target penalty → target penalty + creator reward.
/// - accepted, no target penalty (e.g. `ViolationType::SCORE_BELOW_THRESHOLD`, `DEADLOCK`) → creator reward only.
/// - rejected (false accusation) → creator penalty.
pub fn emergency_score_ops(request: &ConversationUpdateRequest, approved: bool) -> Vec<ScoreOp> {
    let Some(Payload::EmergencyCriteria(ec)) = &request.payload else {
        return Vec::new();
    };
    let Some(evidence) = &ec.evidence else {
        return Vec::new();
    };
    if !approved {
        return vec![creator_penalty(evidence)];
    }
    let mut ops = vec![creator_reward(evidence)];
    if let Some(target_op) = evidence.target_score_op() {
        ops.push(target_op);
    }
    ops
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

    /// Approved + target-mappable violation → creator reward + target penalty.
    #[test]
    fn approved_broken_commit_emits_reward_and_target_penalty() {
        let req = ecp_request(ViolationType::BrokenCommit as i32, vec![0xAA], vec![0xBB]);
        let ops = emergency_score_ops(&req, true);
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
        let ops = emergency_score_ops(&req, true);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Approved `ScoreBelowThreshold` is the trigger for removal — no
    /// target-side score op (the target gets removed instead).
    #[test]
    fn approved_score_below_threshold_emits_reward_only() {
        let req = ecp_request(
            ViolationType::ScoreBelowThreshold as i32,
            vec![0xAA],
            vec![0xBB],
        );
        let ops = emergency_score_ops(&req, true);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Wire-malformed `Unspecified` violation type — emit creator reward,
    /// drop the malformed target op silently.
    #[test]
    fn approved_unspecified_emits_reward_only() {
        let req = ecp_request(0, vec![0xAA], vec![0xBB]);
        let ops = emergency_score_ops(&req, true);
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
            let ops = emergency_score_ops(&req, false);
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].event, ScoreEvent::EmergencyNoCreator);
            assert_eq!(ops[0].member_id, vec![0xBB]);
        }
    }
}
