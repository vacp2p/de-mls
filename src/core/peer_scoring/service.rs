//! Reference [`PeerScoringService`] — a [`PeerScoringPlugin`] implementation
//! over [`PeerScoreStorage`]. Threshold travels with [`ScoringConfig`];
//! per-event score deltas are supplied at construction.

use std::collections::HashMap;

use crate::core::{
    PeerScoreStorage, PeerScoringEvent, PeerScoringPlugin, ScoreEvent, ScoreOp, ScoreSnapshot,
    ScoringConfig,
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
    fn add_member(&mut self, member_id: &[u8]) -> Vec<PeerScoringEvent> {
        let default = self.config.default_score;
        self.storage.set(member_id, default);
        // "Untracked → tracked" treated as "above → new state" for
        // cross-detection purposes, so an unusual config with
        // `default_score <= threshold` still surfaces the new member as
        // a downward cross. The standard config (default 100, threshold
        // 0) silently produces no event.
        if default <= self.config.threshold {
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: member_id.to_vec(),
                score: default,
            }]
        } else {
            Vec::new()
        }
    }

    fn remove_member(&mut self, member_id: &[u8]) {
        self.storage.remove(member_id);
    }

    fn apply_op(&mut self, op: &ScoreOp) -> Vec<PeerScoringEvent> {
        let Some(current) = self.storage.get(&op.member_id) else {
            return Vec::new();
        };
        let delta = self.score_delta(op.event);
        let new_score = current.saturating_add(delta);
        self.storage.set(&op.member_id, new_score);
        cross_event(
            &op.member_id,
            Some(current),
            new_score,
            self.config.threshold,
        )
        .into_iter()
        .collect()
    }

    fn apply_snapshot(&mut self, snapshot: &ScoreSnapshot) -> Vec<PeerScoringEvent> {
        let threshold = self.config.threshold;
        let mut events = Vec::new();
        for (member_id, new_score) in &snapshot.diverged {
            let prior = self.storage.get(member_id);
            self.storage.set(member_id, *new_score);
            if let Some(ev) = cross_event(member_id, prior, *new_score, threshold) {
                events.push(ev);
            }
        }
        events
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

    fn set_default_score(&mut self, score: i64) {
        self.config.default_score = score;
    }
}

/// Compute the [`PeerScoringEvent`] for a transition from `prior` to
/// `new_score`. `prior == None` (untracked) is treated as "above
/// threshold" so a fresh entry landing at-or-below threshold emits a
/// downward cross. Returns `None` when no cross occurred.
fn cross_event(
    member_id: &[u8],
    prior: Option<i64>,
    new_score: i64,
    threshold: i64,
) -> Option<PeerScoringEvent> {
    let was_above = prior.is_none_or(|p| p > threshold);
    let now_below = new_score <= threshold;
    if was_above && now_below {
        Some(PeerScoringEvent::ThresholdCrossedDown {
            member_id: member_id.to_vec(),
            score: new_score,
        })
    } else if !was_above && new_score > threshold {
        Some(PeerScoringEvent::ThresholdCrossedUp {
            member_id: member_id.to_vec(),
            score: new_score,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

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
        let events = svc.add_member(b"alice");
        assert!(events.is_empty(), "default 100 > threshold 0, no cross");
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    #[test]
    fn add_member_with_default_below_threshold_emits_down_event() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            HashMap::new(),
            ScoringConfig {
                default_score: -10,
                threshold: 0,
            },
        );
        let events = svc.add_member(b"alice");
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: b"alice".to_vec(),
                score: -10,
            }]
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
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(events.is_empty(), "100 → 50 stays above threshold 0");
        assert_eq!(svc.score_for(b"alice"), Some(50));
    }

    #[test]
    fn apply_event_unknown_member_returns_no_events() {
        let mut svc = make_service();
        let events = svc.apply_op(&ScoreOp {
            member_id: b"unknown".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(events.is_empty());
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

    #[test]
    fn apply_op_emits_threshold_crossed_up_on_recovery() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        // 100 → 50 → 0 (down), 0 → 20 (up via EmergencyYesCreator).
        for _ in 0..2 {
            let _ = svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            });
        }
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyYesCreator,
        });
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedUp {
                member_id: b"alice".to_vec(),
                score: 20,
            }]
        );
    }

    #[test]
    fn threshold_cross_down_emits_event() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");

        // 100 → 50, still above threshold 0.
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(events.is_empty(), "above threshold, no event");

        // 50 → 0, crosses to at-or-below threshold.
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: b"alice".to_vec(),
                score: 0,
            }]
        );

        // 0 → -50, already below — no further event.
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::BrokenCommit,
        });
        assert!(events.is_empty(), "already below threshold, no event");
    }

    #[test]
    fn apply_ops_concatenates_events_in_order() {
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
        let events = svc.apply_ops(&ops);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            PeerScoringEvent::ThresholdCrossedDown { ref member_id, .. } if member_id == b"alice"
        ));
        assert!(matches!(
            events[1],
            PeerScoringEvent::ThresholdCrossedDown { ref member_id, .. } if member_id == b"bob"
        ));
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
    fn apply_snapshot_emits_event_only_on_actual_cross() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        let snap = ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10), (b"bob".to_vec(), 50)],
        };
        let events = svc.apply_snapshot(&snap);
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: b"alice".to_vec(),
                score: -10,
            }]
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
        let first = svc.apply_snapshot(&snap);
        assert_eq!(first.len(), 1, "first apply emits the cross");
        let second = svc.apply_snapshot(&snap);
        assert!(
            second.is_empty(),
            "second apply on unchanged state emits nothing"
        );
    }

    #[test]
    fn apply_snapshot_emits_threshold_crossed_up_on_recovery() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        // First push alice below threshold.
        let _ = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        });
        // Now snapshot her back above.
        let events = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), 50)],
        });
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedUp {
                member_id: b"alice".to_vec(),
                score: 50,
            }]
        );
    }

    #[test]
    fn apply_snapshot_for_untracked_below_threshold_emits_down() {
        // Untracked → tracked (below threshold) treated as a downward
        // cross from "above-by-default."
        let mut svc = make_service();
        let events = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"newcomer".to_vec(), -10)],
        });
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: b"newcomer".to_vec(),
                score: -10,
            }]
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
