//! Reference [`PeerScoringService`] — a [`PeerScoringPlugin`] implementation
//! over [`PeerScoreStorage`]. Threshold travels with [`ScoringConfig`];
//! per-event score deltas are supplied at construction.

use std::collections::HashMap;

use crate::core::{
    PeerScoreStorage, PeerScoringPlugin, ScoreEvent, ScoreOp, ScoreSnapshot, ScoringConfig,
};

/// Per-conversation, per-member score tracker. Reference [`PeerScoringPlugin`]
/// implementation. One instance per conversation; threshold travels with
/// [`ScoringConfig`]. Storage is abstracted via [`PeerScoreStorage`] so
/// app-layer backends (in-memory, on-disk, …) plug in without touching
/// this protocol logic.
pub struct PeerScoringService<S: PeerScoreStorage> {
    storage: S,
    score_deltas: HashMap<ScoreEvent, i64>,
    config: ScoringConfig,
}

impl<S: PeerScoreStorage> PeerScoringService<S> {
    pub fn new(storage: S, score_deltas: HashMap<ScoreEvent, i64>, config: ScoringConfig) -> Self {
        Self {
            storage,
            score_deltas,
            config,
        }
    }

    /// Signed score delta for `event`. Events not in the table contribute 0.
    fn score_delta(&self, event: ScoreEvent) -> i64 {
        self.score_deltas.get(&event).copied().unwrap_or(0)
    }
}

impl<S: PeerScoreStorage> PeerScoringPlugin for PeerScoringService<S> {
    fn add_member(&mut self, member_id: &[u8]) -> bool {
        let default = self.config.default_score;
        self.storage.set(member_id, default);
        // "Untracked → tracked" treated as "above → new state": an unusual
        // config with `default_score <= threshold` surfaces the new member
        // as a downward cross. The standard config (default 100, threshold
        // 0) returns false.
        default <= self.config.threshold
    }

    fn remove_member(&mut self, member_id: &[u8]) {
        self.storage.remove(member_id);
    }

    /// Apply an incremental delta to an already-tracked member. Unlike
    /// `add_member` / `apply_snapshot`, this never creates an entry — a
    /// stale op must not resurrect a removed member. The coordinator
    /// `add_member`s first; a drop means roster and scores are out of sync.
    fn apply_op(&mut self, op: &ScoreOp) -> bool {
        let Some(current) = self.storage.get(&op.member_id) else {
            tracing::debug!(
                member = ?op.member_id,
                event = ?op.event,
                "score op dropped: member not tracked (add_member first)"
            );
            return false;
        };
        let delta = self.score_delta(op.event);
        let new_score = current.saturating_add(delta);
        self.storage.set(&op.member_id, new_score);
        crossed_down(Some(current), new_score, self.config.threshold)
    }

    fn apply_snapshot(&mut self, snapshot: &ScoreSnapshot) -> bool {
        let threshold = self.config.threshold;
        let mut crossed = false;
        for (member_id, new_score) in &snapshot.diverged {
            let prior = self.storage.get(member_id);
            self.storage.set(member_id, *new_score);
            crossed |= crossed_down(prior, *new_score, threshold);
        }
        crossed
    }

    fn snapshot(&self) -> ScoreSnapshot {
        let default = self.config.default_score;
        let diverged = self
            .storage
            .all_scores()
            .into_iter()
            .filter(|(_, score)| *score != default)
            .collect();
        ScoreSnapshot { diverged }
    }

    fn score_for(&self, member_id: &[u8]) -> Option<i64> {
        self.storage.get(member_id)
    }

    fn members_below_threshold(&self) -> Vec<Vec<u8>> {
        let threshold = self.config.threshold;
        self.storage
            .all_scores()
            .into_iter()
            .filter(|(_, score)| *score <= threshold)
            .map(|(id, _)| id)
            .collect()
    }

    fn all_members_with_scores(&self) -> Vec<(Vec<u8>, i64)> {
        self.storage.all_scores()
    }

    fn threshold(&self) -> i64 {
        self.config.threshold
    }

    fn set_threshold(&mut self, threshold: i64) {
        self.config.threshold = threshold;
    }

    fn default_score(&self) -> i64 {
        self.config.default_score
    }
}

/// `true` when `new_score` crosses a member *down* to at-or-below
/// `threshold` from above. `prior == None` (untracked) counts as "above",
/// so a fresh entry landing at-or-below threshold is a downward cross.
/// Upward recovery is not surfaced — no coordinator consumes it.
fn crossed_down(prior: Option<i64>, new_score: i64, threshold: i64) -> bool {
    let was_above = prior.is_none_or(|p| p > threshold);
    let now_below = new_score <= threshold;
    was_above && now_below
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    // ── Test scaffolding ────────────────────────────────────────────

    /// Minimal in-memory storage for service tests. Production storage
    /// lives in [`crate::session::InMemoryPeerScoreStorage`].
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

    fn make_service() -> PeerScoringService<TestStorage> {
        let deltas = HashMap::from([
            (ScoreEvent::EmergencyNoCreator, -50),
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::SuccessfulCommit, 10),
            (ScoreEvent::MisbehavingCommit, -30),
        ]);
        PeerScoringService::new(
            TestStorage::default(),
            deltas,
            ScoringConfig {
                default_score: 100,
                threshold: 0,
            },
        )
    }

    #[test]
    fn add_member_gets_default_score() {
        let mut svc = make_service();
        let crossed = svc.add_member(b"alice");
        assert!(!crossed, "default 100 > threshold 0, no cross");
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    #[test]
    fn add_member_below_threshold_crosses_down() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            HashMap::new(),
            ScoringConfig {
                default_score: -10,
                threshold: 0,
            },
        );
        assert!(
            svc.add_member(b"alice"),
            "default -10 <= threshold 0 crosses down"
        );
    }

    #[test]
    fn unknown_member_returns_none() {
        let svc = make_service();
        assert_eq!(svc.score_for(b"unknown"), None);
    }

    #[test]
    fn remove_member_clears_score() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        svc.remove_member(b"alice");
        assert_eq!(svc.score_for(b"alice"), None);
    }

    #[test]
    fn apply_event_decreases_score() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let crossed = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(!crossed, "100 → 50 stays above threshold 0");
        assert_eq!(svc.score_for(b"alice"), Some(50));
    }

    #[test]
    fn apply_op_unknown_member_returns_false() {
        let mut svc = make_service();
        let crossed = svc.apply_op(&ScoreOp {
            member_id: b"unknown".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(!crossed);
    }

    #[test]
    fn multiple_events_accumulate() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        for event in [
            ScoreEvent::EmergencyNoCreator,
            ScoreEvent::MisbehavingCommit,
            ScoreEvent::SuccessfulCommit,
        ] {
            let _ = svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event,
            });
        }
        assert_eq!(svc.score_for(b"alice"), Some(30));
    }

    /// Recovery back above threshold emits no event — only downward
    /// crosses are surfaced (no coordinator consumes upward recovery).
    #[test]
    fn apply_op_recovery_above_threshold_no_cross() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        // 100 → 50 → 0 (down emitted at the 0 cross), 0 → 20 (recovery).
        for _ in 0..2 {
            let _ = svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            });
        }
        let crossed = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyYesCreator,
        });
        assert!(!crossed, "upward recovery cross is not surfaced");
    }

    #[test]
    fn apply_op_crosses_down_returns_true() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");

        // 100 → 50, still above threshold 0.
        let crossed = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(!crossed, "above threshold, no cross");

        // 50 → 0, crosses to at-or-below threshold.
        let crossed = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(crossed, "0 is at-or-below threshold");

        // 0 → -50, already below — no further cross.
        let crossed = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::BrokenCommit,
        });
        assert!(!crossed, "already below threshold, no cross");
    }

    #[test]
    fn apply_ops_true_when_a_member_crosses_and_applies_all() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        // Drop alice across threshold (-50 + -50 = 0 ≤ 0) and bob too in
        // the same batch.
        let ops = vec![
            ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
            ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
            ScoreOp {
                member_id: b"bob".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
            ScoreOp {
                member_id: b"bob".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
        ];
        assert!(svc.apply_ops(&ops), "a member crossed down in the batch");
        // Every op applied (not short-circuited): both land at/below threshold.
        let below = svc.members_below_threshold();
        assert!(below.contains(&b"alice".to_vec()));
        assert!(below.contains(&b"bob".to_vec()));
    }

    #[test]
    fn snapshot_includes_only_diverged_scores() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        let _ = svc.add_member(b"charlie");
        let _ = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });
        let snap = svc.snapshot();
        let ids: Vec<&[u8]> = snap.diverged.iter().map(|(id, _)| id.as_slice()).collect();
        assert_eq!(ids, vec![b"alice".as_slice()]);
        assert_eq!(snap.diverged[0].1, 110);
    }

    #[test]
    fn apply_snapshot_crosses_only_on_actual_cross() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        let snap = ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10), (b"bob".to_vec(), 50)],
        };
        assert!(
            svc.apply_snapshot(&snap),
            "alice crosses down, bob stays above"
        );
        assert_eq!(svc.score_for(b"alice"), Some(-10));
        assert_eq!(svc.score_for(b"bob"), Some(50));
    }

    #[test]
    fn apply_snapshot_idempotent_on_repeat() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let snap = ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        };
        assert!(svc.apply_snapshot(&snap), "first apply crosses down");
        assert!(
            !svc.apply_snapshot(&snap),
            "second apply on unchanged state does not cross"
        );
    }

    /// A snapshot moving a member back above threshold emits no event —
    /// only downward crosses are surfaced.
    #[test]
    fn apply_snapshot_recovery_above_threshold_no_cross() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        // First push alice below threshold.
        let _ = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        });
        // Now snapshot her back above.
        let crossed = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), 50)],
        });
        assert!(!crossed, "upward recovery cross is not surfaced");
    }

    #[test]
    fn apply_snapshot_untracked_below_threshold_crosses_down() {
        // Untracked → tracked (below threshold) treated as a downward
        // cross from "above-by-default."
        let mut svc = make_service();
        assert!(
            svc.apply_snapshot(&ScoreSnapshot {
                diverged: vec![(b"newcomer".to_vec(), -10)],
            }),
            "untracked entry below threshold crosses down"
        );
    }

    #[test]
    fn members_below_threshold_filters_correctly() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        let _ = svc.add_member(b"charlie");
        for event in [ScoreEvent::EmergencyNoCreator, ScoreEvent::BrokenCommit] {
            let _ = svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event,
            });
        }
        for _ in 0..2 {
            let _ = svc.apply_op(&ScoreOp {
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
        let _ = svc.add_member(b"alice");
        // Apply via snapshot to set an absolute score without going
        // through the delta table.
        let _ = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        });

        svc.set_threshold(-50);
        assert!(!svc.members_below_threshold().contains(&b"alice".to_vec()));

        svc.set_threshold(-5);
        assert!(svc.members_below_threshold().contains(&b"alice".to_vec()));
    }

    #[test]
    fn score_saturates_no_overflow() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)]),
            ScoringConfig {
                default_score: i64::MAX,
                threshold: 0,
            },
        );
        let _ = svc.add_member(b"alice");
        let _ = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });
        assert_eq!(svc.score_for(b"alice"), Some(i64::MAX));
    }

    #[test]
    fn unknown_event_yields_zero_delta() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            HashMap::from([(ScoreEvent::EmergencyNoCreator, -50)]),
            ScoringConfig {
                default_score: 100,
                threshold: 0,
            },
        );
        let _ = svc.add_member(b"alice");
        let _ = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }
}
