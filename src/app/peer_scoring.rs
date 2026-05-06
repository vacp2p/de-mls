//! [`PeerScoringService`] plus default in-memory backends. One instance
//! per group; scores are keyed by `member_id` via a [`PeerScoreStorage`]
//! and mapped from events via a [`ScoringProvider`].

use std::collections::HashMap;

use crate::core::{PeerScoreStorage, ScoreEvent, ScoreOp, ScoringConfig, ScoringProvider};

// ── In-memory storage ───────────────────────────────────────────────

/// `HashMap`-backed [`PeerScoreStorage`] for tests and simple deployments.
/// Production integrators should supply a durable backend.
#[derive(Debug, Clone, Default)]
pub struct InMemoryPeerScoreStorage {
    scores: HashMap<Vec<u8>, i64>,
}

impl InMemoryPeerScoreStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PeerScoreStorage for InMemoryPeerScoreStorage {
    fn get(&self, member_id: &[u8]) -> Option<i64> {
        self.scores.get(member_id).copied()
    }

    fn set(&mut self, member_id: &[u8], score: i64) {
        self.scores.insert(member_id.to_vec(), score);
    }

    fn remove(&mut self, member_id: &[u8]) {
        self.scores.remove(member_id);
    }

    fn all_scores(&self) -> Vec<(Vec<u8>, i64)> {
        self.scores.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

// ── Default provider ───────────────────────────────────────────────

/// Constant [`ScoringProvider`] — events not in the map produce a delta of 0.
#[derive(Debug, Clone)]
pub struct FixedScoringProvider {
    deltas: HashMap<ScoreEvent, i64>,
}

impl FixedScoringProvider {
    pub fn new(deltas: HashMap<ScoreEvent, i64>) -> Self {
        Self { deltas }
    }

    /// Default delta for each [`ScoreEvent`]. Values are placeholders
    /// pending an empirical tuning pass (see `docs/ROADMAP.md`).
    pub fn default_deltas() -> HashMap<ScoreEvent, i64> {
        HashMap::from([
            // ECP target penalties (violation-type-specific)
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::BrokenMlsProposal, -30),
            (ScoreEvent::CensorshipInactivity, -40),
            // ECP creator outcomes
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::EmergencyNoCreator, -50),
            // Commit selection
            (ScoreEvent::SuccessfulCommit, 10),
            (ScoreEvent::HonestCommitAttempt, 5),
            (ScoreEvent::MisbehavingCommit, -30),
        ])
    }

    /// Convenience constructor: [`Self::new`] with [`Self::default_deltas`].
    pub fn with_default_deltas() -> Self {
        Self::new(Self::default_deltas())
    }
}

impl ScoringProvider for FixedScoringProvider {
    fn score_delta(&self, event: ScoreEvent) -> i64 {
        self.deltas.get(&event).copied().unwrap_or(0)
    }
}

// ── Service ─────────────────────────────────────────────────────────

/// Per-group, per-member score tracker. One instance per group;
/// threshold travels with [`ScoringConfig`].
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
    use super::*;

    fn default_config() -> ScoringConfig {
        ScoringConfig {
            default_score: 100,
            threshold: 0,
        }
    }

    fn make_service() -> PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider> {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::with_default_deltas(),
            default_config(),
        )
    }

    #[test]
    fn test_add_member_gets_default_score() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    #[test]
    fn test_unknown_member_returns_none() {
        let svc = make_service();
        assert_eq!(svc.score_for(b"unknown"), None);
    }

    #[test]
    fn test_remove_member() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.remove_member(b"alice");
        assert_eq!(svc.score_for(b"alice"), None);
    }

    #[test]
    fn test_apply_event_decreases_score() {
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
    fn test_apply_event_increases_score() {
        let mut svc = make_service();
        svc.add_member(b"alice");

        let new_score = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });

        assert_eq!(new_score, Some(110));
    }

    #[test]
    fn test_apply_event_unknown_member_returns_none() {
        let mut svc = make_service();

        let result = svc.apply_op(&ScoreOp {
            member_id: b"unknown".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });

        assert_eq!(result, None);
    }

    #[test]
    fn test_multiple_events_accumulate() {
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
    fn test_members_below_threshold() {
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
        assert_eq!(below.len(), 2);
        assert!(below.contains(&b"alice".to_vec()));
        assert!(below.contains(&b"charlie".to_vec()));
        assert!(!below.contains(&b"bob".to_vec()));
    }

    #[test]
    fn test_is_below_threshold() {
        let mut svc = make_service();
        svc.add_member(b"alice");

        assert!(!svc.is_below_threshold(b"alice"));

        for event in [ScoreEvent::EmergencyNoCreator, ScoreEvent::BrokenCommit] {
            svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event,
            });
        }

        assert!(svc.is_below_threshold(b"alice"));
    }

    #[test]
    fn test_is_below_threshold_unknown_member() {
        let svc = make_service();
        assert!(!svc.is_below_threshold(b"unknown"));
    }

    #[test]
    fn test_score_saturates_no_overflow() {
        let mut svc = PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::new(HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)])),
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
    fn test_unknown_event_in_provider_returns_zero_delta() {
        let mut svc = PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::new(HashMap::from([(ScoreEvent::EmergencyNoCreator, -50)])),
            default_config(),
        );
        svc.add_member(b"alice");

        let new_score = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });

        assert_eq!(new_score, Some(100));
    }

    #[test]
    fn test_determinism_independent_instances() {
        let events = vec![
            (b"alice".as_slice(), ScoreEvent::EmergencyNoCreator),
            (b"alice".as_slice(), ScoreEvent::SuccessfulCommit),
            (b"bob".as_slice(), ScoreEvent::BrokenCommit),
            (b"bob".as_slice(), ScoreEvent::SuccessfulCommit),
        ];

        let mut svc1 = make_service();
        let mut svc2 = make_service();

        for svc in [&mut svc1, &mut svc2] {
            svc.add_member(b"alice");
            svc.add_member(b"bob");
        }

        for (member, event) in &events {
            for svc in [&mut svc1, &mut svc2] {
                svc.apply_op(&ScoreOp {
                    member_id: member.to_vec(),
                    event: *event,
                });
            }
        }

        assert_eq!(svc1.score_for(b"alice"), svc2.score_for(b"alice"));
        assert_eq!(svc1.score_for(b"bob"), svc2.score_for(b"bob"));
        assert_eq!(
            svc1.members_below_threshold().len(),
            svc2.members_below_threshold().len()
        );
    }

    #[test]
    fn test_false_accusation_penalty() {
        let mut svc = make_service();
        svc.add_member(b"accuser");
        svc.add_member(b"target");

        svc.apply_op(&ScoreOp {
            member_id: b"accuser".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });

        assert_eq!(svc.score_for(b"accuser"), Some(50));
        assert_eq!(svc.score_for(b"target"), Some(100));
    }
}
