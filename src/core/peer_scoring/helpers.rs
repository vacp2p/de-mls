//! Pure helpers: scoring/member-roster diff and ECP score-op derivation.
//! No state, no I/O.

use std::collections::HashSet;

use prost::Message;

use crate::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, ViolationEvidence, conversation_update_request::Payload,
};

use crate::core::{ScoreEvent, ScoreOp, ScoringMemberDiff};

/// Diff between a scoring table snapshot and an MLS member roster.
/// Caller applies the diff to its own [`super::PeerScoringPlugin`].
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

/// Score ops to apply when an emergency proposal resolves. Returns an
/// empty vector when the payload isn't an ECP or has no evidence.
///
/// - accepted target-bearing emergency → target penalty + creator reward.
/// - accepted `SCORE_BELOW_THRESHOLD` or `DEADLOCK` → creator reward only.
/// - rejected emergency → creator penalty.
pub fn emergency_score_ops(payload: &[u8], approved: bool) -> Vec<ScoreOp> {
    let Ok(req) = ConversationUpdateRequest::decode(payload) else {
        return Vec::new();
    };
    let Some(Payload::EmergencyCriteria(ec)) = req.payload else {
        return Vec::new();
    };
    let Some(evidence) = ec.evidence else {
        return Vec::new();
    };

    if approved {
        let mut ops = vec![creator_reward(&evidence)];
        if let Some(target_op) = evidence.target_score_op() {
            ops.push(target_op);
        }
        ops
    } else {
        vec![creator_penalty(&evidence)]
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

    fn ecp_payload(violation_type: i32, target: Vec<u8>, creator: Vec<u8>) -> Vec<u8> {
        let evidence = ViolationEvidence {
            violation_type,
            target_member_id: target,
            evidence_payload: Vec::new(),
            epoch: 0,
            creator_member_id: creator,
        };
        let req = ConversationUpdateRequest {
            payload: Some(Payload::EmergencyCriteria(EmergencyCriteriaProposal {
                evidence: Some(evidence),
            })),
        };
        req.encode_to_vec()
    }

    /// Approved + target-mappable violation → creator reward + target penalty.
    #[test]
    fn approved_broken_commit_emits_reward_and_target_penalty() {
        let payload = ecp_payload(ViolationType::BrokenCommit as i32, vec![0xAA], vec![0xBB]);
        let ops = emergency_score_ops(&payload, true);
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
        assert_eq!(ops[0].member_id, vec![0xBB]);
        assert_eq!(ops[1].event, ScoreEvent::BrokenCommit);
        assert_eq!(ops[1].member_id, vec![0xAA]);
    }

    /// Approved + non-target violation (`Deadlock`) → creator reward only.
    #[test]
    fn approved_deadlock_emits_reward_only() {
        let payload = ecp_payload(ViolationType::Deadlock as i32, Vec::new(), vec![0xBB]);
        let ops = emergency_score_ops(&payload, true);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Approved `ScoreBelowThreshold` is the trigger for removal — no
    /// target-side score op (the target gets removed instead).
    #[test]
    fn approved_score_below_threshold_emits_reward_only() {
        let payload = ecp_payload(
            ViolationType::ScoreBelowThreshold as i32,
            vec![0xAA],
            vec![0xBB],
        );
        let ops = emergency_score_ops(&payload, true);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Wire-malformed `Unspecified` violation type — emit creator reward,
    /// drop the malformed target op silently.
    #[test]
    fn approved_unspecified_emits_reward_only() {
        let payload = ecp_payload(0, vec![0xAA], vec![0xBB]);
        let ops = emergency_score_ops(&payload, true);
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
            let payload = ecp_payload(vt as i32, vec![0xAA], vec![0xBB]);
            let ops = emergency_score_ops(&payload, false);
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].event, ScoreEvent::EmergencyNoCreator);
            assert_eq!(ops[0].member_id, vec![0xBB]);
        }
    }
}
