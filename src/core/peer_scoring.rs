//! Peer-scoring vocabulary, traits, reference [`PeerScoringService`],
//! and pure score-derivation helpers. The service is storage- and
//! policy-agnostic; concrete backends (e.g.
//! [`crate::app::InMemoryPeerScoreStorage`]) and delta-table providers
//! (e.g. [`crate::app::FixedScoringProvider`]) live in the app layer.

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
    /// At or below this score, a member is eligible for
    /// `SCORE_BELOW_THRESHOLD` ECP removal (RFC §Peer Scoring).
    pub threshold: i64,
}

/// Default removal threshold (RFC §Peer Scoring `threshold_peer_score`).
pub const DEFAULT_THRESHOLD_PEER_SCORE: i64 = 0;

/// Per-member score persistence for a single group. The app layer ships
/// an in-memory default impl. One storage instance per group — no
/// `group_id` keying.
pub trait PeerScoreStorage {
    fn get(&self, member_id: &[u8]) -> Option<i64>;
    fn set(&mut self, member_id: &[u8], score: i64);
    fn remove(&mut self, member_id: &[u8]);
    fn all_scores(&self) -> Vec<(Vec<u8>, i64)>;
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
/// Caller applies the diff to its own [`PeerScoringService`].
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

// ── Reference scoring service ───────────────────────────────────────

/// Per-group, per-member score tracker. One instance per group;
/// threshold travels with [`ScoringConfig`]. Storage is abstracted via
/// [`PeerScoreStorage`] so app-layer backends (in-memory, on-disk, …)
/// plug in without touching this protocol logic.
pub struct PeerScoringService<S: PeerScoreStorage, P: ScoringProvider> {
    storage: S,
    provider: P,
    config: ScoringConfig,
}

impl<S: PeerScoreStorage, P: ScoringProvider> PeerScoringService<S, P> {
    pub fn new(storage: S, provider: P, config: ScoringConfig) -> Self {
        Self {
            storage,
            provider,
            config,
        }
    }

    /// Start tracking a member with the default score.
    pub fn add_member(&mut self, member_id: &[u8]) {
        self.storage.set(member_id, self.config.default_score);
    }

    pub fn remove_member(&mut self, member_id: &[u8]) {
        self.storage.remove(member_id);
    }

    /// Apply a single [`ScoreOp`] and return the target's new score, or
    /// `None` if the target isn't tracked.
    pub fn apply_op(&mut self, op: &ScoreOp) -> Option<i64> {
        let current = self.storage.get(&op.member_id)?;
        let delta = self.provider.score_delta(op.event);
        let new_score = current.saturating_add(delta);
        self.storage.set(&op.member_id, new_score);
        Some(new_score)
    }

    /// Apply a batch of [`ScoreOp`]s. Untracked targets are skipped
    /// silently (same semantics as [`Self::apply_op`]).
    pub fn apply_ops(&mut self, ops: &[ScoreOp]) {
        for op in ops {
            self.apply_op(op);
        }
    }

    pub fn score_for(&self, member_id: &[u8]) -> Option<i64> {
        self.storage.get(member_id)
    }

    /// Force-set a score; used when applying a `GroupSync` from the steward.
    pub fn set_score(&mut self, member_id: &[u8], score: i64) {
        self.storage.set(member_id, score);
    }

    pub fn members_below_threshold(&self) -> Vec<Vec<u8>> {
        let threshold = self.config.threshold;
        self.storage
            .all_scores()
            .into_iter()
            .filter(|(_, score)| *score <= threshold)
            .map(|(id, _)| id)
            .collect()
    }

    pub fn is_below_threshold(&self, member_id: &[u8]) -> bool {
        self.storage
            .get(member_id)
            .is_some_and(|s| s <= self.config.threshold)
    }

    pub fn all_members_with_scores(&self) -> Vec<(Vec<u8>, i64)> {
        self.storage.all_scores()
    }

    pub fn config(&self) -> &ScoringConfig {
        &self.config
    }

    pub fn set_threshold(&mut self, threshold: i64) {
        self.config.threshold = threshold;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::protos::de_mls::messages::v1::{EmergencyCriteriaProposal, ViolationType};

    // ── Test scaffolding ────────────────────────────────────────────

    /// Minimal in-memory storage for service tests. Production storage
    /// lives in [`crate::app::InMemoryPeerScoreStorage`].
    #[derive(Default)]
    struct TestStorage(HashMap<Vec<u8>, i64>);

    impl PeerScoreStorage for TestStorage {
        fn get(&self, member_id: &[u8]) -> Option<i64> {
            self.0.get(member_id).copied()
        }
        fn set(&mut self, member_id: &[u8], score: i64) {
            self.0.insert(member_id.to_vec(), score);
        }
        fn remove(&mut self, member_id: &[u8]) {
            self.0.remove(member_id);
        }
        fn all_scores(&self) -> Vec<(Vec<u8>, i64)> {
            self.0.iter().map(|(k, v)| (k.clone(), *v)).collect()
        }
    }

    /// HashMap-backed [`ScoringProvider`] with caller-supplied deltas.
    /// Production deltas live in [`crate::app::FixedScoringProvider`].
    struct TestProvider(HashMap<ScoreEvent, i64>);

    impl ScoringProvider for TestProvider {
        fn score_delta(&self, event: ScoreEvent) -> i64 {
            self.0.get(&event).copied().unwrap_or(0)
        }
    }

    fn make_service() -> PeerScoringService<TestStorage, TestProvider> {
        let deltas = HashMap::from([
            (ScoreEvent::EmergencyNoCreator, -50),
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::SuccessfulCommit, 10),
            (ScoreEvent::MisbehavingCommit, -30),
        ]);
        PeerScoringService::new(
            TestStorage::default(),
            TestProvider(deltas),
            ScoringConfig {
                default_score: 100,
                threshold: 0,
            },
        )
    }

    // ── Service tests ────────────────────────────────────────────────

    #[test]
    fn add_member_gets_default_score() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    #[test]
    fn unknown_member_returns_none() {
        let svc = make_service();
        assert_eq!(svc.score_for(b"unknown"), None);
    }

    #[test]
    fn remove_member_clears_score() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.remove_member(b"alice");
        assert_eq!(svc.score_for(b"alice"), None);
    }

    #[test]
    fn apply_event_decreases_score() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        let new_score = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert_eq!(new_score, Some(50));
        assert_eq!(svc.score_for(b"alice"), Some(50));
    }

    #[test]
    fn apply_event_unknown_member_returns_none() {
        let mut svc = make_service();
        let result = svc.apply_op(&ScoreOp {
            member_id: b"unknown".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert_eq!(result, None);
    }

    #[test]
    fn multiple_events_accumulate() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        for event in [
            ScoreEvent::EmergencyNoCreator,
            ScoreEvent::MisbehavingCommit,
            ScoreEvent::SuccessfulCommit,
        ] {
            svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event,
            });
        }
        assert_eq!(svc.score_for(b"alice"), Some(30));
    }

    #[test]
    fn members_below_threshold_filters_correctly() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.add_member(b"bob");
        svc.add_member(b"charlie");
        for event in [ScoreEvent::EmergencyNoCreator, ScoreEvent::BrokenCommit] {
            svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event,
            });
        }
        for _ in 0..2 {
            svc.apply_op(&ScoreOp {
                member_id: b"charlie".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            });
        }
        let below = svc.members_below_threshold();
        assert!(below.contains(&b"alice".to_vec()));
        assert!(below.contains(&b"charlie".to_vec()));
        assert!(!below.contains(&b"bob".to_vec()));
    }

    #[test]
    fn set_threshold_changes_below_threshold_set() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.set_score(b"alice", -10);

        svc.set_threshold(-50);
        assert!(!svc.members_below_threshold().contains(&b"alice".to_vec()));

        svc.set_threshold(-5);
        assert!(svc.members_below_threshold().contains(&b"alice".to_vec()));
    }

    #[test]
    fn score_saturates_no_overflow() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            TestProvider(HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)])),
            ScoringConfig {
                default_score: i64::MAX,
                threshold: 0,
            },
        );
        svc.add_member(b"alice");
        let new_score = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });
        assert_eq!(new_score, Some(i64::MAX));
    }

    #[test]
    fn unknown_event_yields_zero_delta() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            TestProvider(HashMap::from([(ScoreEvent::EmergencyNoCreator, -50)])),
            ScoringConfig {
                default_score: 100,
                threshold: 0,
            },
        );
        svc.add_member(b"alice");
        let new_score = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });
        assert_eq!(new_score, Some(100));
    }

    // ── ECP score derivation tests ──────────────────────────────────

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
