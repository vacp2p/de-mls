//! Types and traits core uses to describe scoring-relevant events plus
//! pure score-derivation helpers. The scoring service itself lives in
//! `crate::app::peer_scoring` and consumes the `Vec<ScoreOp>` that the
//! helpers here return.

use prost::Message;

use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, ViolationEvidence, group_update_request::Payload,
};

// ── Score events ────────────────────────────────────────────────────

/// A scoreable event in the protocol.
///
/// Each variant maps to a single score delta. Violation types (BrokenCommit, etc.)
/// go through the ECP consensus path — when accepted, the target receives a
/// violation-type-specific penalty and the creator receives a flat reward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScoreEvent {
    // ── ECP target penalties (mapped from ViolationType in evidence) ──
    /// ECP accepted: steward committed proposals that don't match what was voted on.
    BrokenCommit,
    /// ECP accepted: MLS proposal payload was malformed or didn't match the voted action.
    BrokenMlsProposal,
    /// ECP accepted: steward failed to commit within the threshold duration.
    CensorshipInactivity,

    // ── ECP creator outcomes ──
    /// ECP accepted — flat reward to the proposal creator.
    EmergencyYesCreator,
    /// ECP rejected — flat penalty to the proposal creator (false accusation).
    EmergencyNoCreator,

    // ── Commit selection ──
    /// Steward successfully committed a valid batch.
    SuccessfulCommit,
    /// Competing commit with same proposals but different MLS entropy — honest
    /// participation (RFC: "MUST NOT be classified as misbehavior").
    HonestCommitAttempt,
    /// Competing commit with a different proposal set than the selected one
    /// (RFC: "MUST be classified as misbehavior").
    MisbehavingCommit,
}

/// A score operation produced by core logic. The app layer feeds these
/// into its scoring service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreOp {
    pub member_id: Vec<u8>,
    pub event: ScoreEvent,
}

// ── Scoring configuration ───────────────────────────────────────────

/// Maps each [`ScoreEvent`] to a signed score delta (positive = reward).
/// The app layer ships a fixed-table default impl.
pub trait ScoringProvider {
    fn score_delta(&self, event: ScoreEvent) -> i64;
}

#[derive(Debug, Clone)]
pub struct ScoringConfig {
    /// Score assigned to newly added members.
    pub default_score: i64,
}

/// Per-(group, member) score persistence. The app layer ships an
/// in-memory default impl.
pub trait PeerScoreStorage {
    fn get(&self, group_id: &str, member_id: &[u8]) -> Option<i64>;
    fn set(&mut self, group_id: &str, member_id: &[u8], score: i64);
    fn remove(&mut self, group_id: &str, member_id: &[u8]);
    fn all_scores(&self, group_id: &str) -> Vec<(Vec<u8>, i64)>;

    /// Drop every score entry for `group_id`. Called on leave so a future
    /// rejoin starts from a clean per-group table populated by the new
    /// `GroupSync` rather than carrying stale entries from the prior
    /// session.
    fn remove_group(&mut self, group_id: &str);
}

// ── Scoring-member diff ─────────────────────────────────────────────

/// Members to add to and remove from a scoring table to bring it into
/// sync with the current MLS membership.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScoringMemberDiff {
    pub to_add: Vec<Vec<u8>>,
    pub to_remove: Vec<Vec<u8>>,
}

/// Pure diff between a scoring table snapshot and an MLS member roster.
/// Caller applies the diff to its own
/// [`PeerScoringService`](crate::app::PeerScoringService).
pub fn scoring_member_diff(scored: &[Vec<u8>], mls_members: &[Vec<u8>]) -> ScoringMemberDiff {
    let scored_set: std::collections::HashSet<&[u8]> = scored.iter().map(Vec::as_slice).collect();
    let mls_set: std::collections::HashSet<&[u8]> = mls_members.iter().map(Vec::as_slice).collect();

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

// ── ECP score derivation ────────────────────────────────────────────

/// Score ops to apply when an emergency proposal resolves. Returns an
/// empty vector when the payload isn't an ECP or has no evidence.
///
/// - accepted target-bearing emergency → target penalty + creator reward.
/// - accepted `SCORE_BELOW_THRESHOLD` or `DEADLOCK` → creator reward only.
/// - rejected emergency → creator penalty.
pub fn emergency_score_ops(payload: &[u8], approved: bool) -> Vec<ScoreOp> {
    let Ok(req) = GroupUpdateRequest::decode(payload) else {
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
        let req = GroupUpdateRequest {
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
