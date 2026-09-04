//! [`PeerScoringService`] — the engine's per-conversation peer-score
//! tracker. It owns the RFC scoring logic (delta application, snapshot
//! bootstrap, change reporting) and the score table itself: a plain
//! `member_id -> score` map that cannot fail to read or write.

use std::collections::HashMap;

use crate::{ConversationError, ScoreChange, ScoreEvent, ScoreOp, ScoreSnapshot, ScoringConfig};

/// Tracks peer scores for a conversation. Applies delta changes, records results,
/// but doesn't enforce thresholds — that's up to the integrator.
pub struct PeerScoringService {
    scores: HashMap<Vec<u8>, i64>,
    score_deltas: HashMap<ScoreEvent, i64>,
    config: ScoringConfig,
}

impl PeerScoringService {
    /// You can override just the score events you want — any you leave out use their default values.
    pub fn new(score_delta_overrides: HashMap<ScoreEvent, i64>, config: ScoringConfig) -> Self {
        let mut score_deltas = crate::default_score_deltas();
        score_deltas.extend(score_delta_overrides);
        Self {
            scores: HashMap::new(),
            score_deltas,
            config,
        }
    }

    /// Returns the score change for an event—uses your override if set.
    /// Always succeeds since all events used are present in the map.
    fn score_delta(&self, event: ScoreEvent) -> i64 {
        self.score_deltas.get(&event).copied().unwrap()
    }

    /// Start tracking a member at the configured default score.
    pub(crate) fn add_member(&mut self, member_id: &[u8]) {
        self.scores
            .insert(member_id.to_vec(), self.config.default_score);
    }

    /// Stop tracking a member. Idempotent on an untracked member.
    pub(crate) fn remove_member(&mut self, member_id: &[u8]) {
        self.scores.remove(member_id);
    }

    /// Applies a batch of [`ScoreOp`]s and returns how each affected member's score changed.
    ///
    /// For each member, only the net result is shown—multiple changes cancel out if they sum to zero.
    /// If a member isn’t tracked, the op is ignored.
    ///
    /// Each member’s score is looked up once, and all updates are stored in order of first change.
    pub(crate) fn apply_ops(&mut self, ops: &[ScoreOp]) -> Vec<ScoreChange> {
        // (member, score before the batch, score so far), in first-touch order.
        let mut touched: Vec<(Vec<u8>, i64, i64)> = Vec::new();

        for op in ops {
            let index = match touched.iter().position(|(id, _, _)| *id == op.member_id) {
                Some(index) => index,
                None => {
                    // Don't add missing members here; ignore ops for members not tracked.
                    let Some(&score) = self.scores.get(&op.member_id) else {
                        tracing::debug!(
                            member = ?op.member_id,
                            event = ?op.event,
                            "score op dropped: member not tracked (add_member first)"
                        );
                        continue;
                    };
                    touched.push((op.member_id.clone(), score, score));
                    touched.len() - 1
                }
            };

            let score = touched[index].2.saturating_add(self.score_delta(op.event));
            touched[index].2 = score;
            self.scores.insert(op.member_id.clone(), score);
        }

        touched
            .into_iter()
            .filter(|(_, previous, score)| previous != score)
            .map(|(member_id, previous, score)| ScoreChange {
                member_id,
                previous,
                score,
            })
            .collect()
    }

    /// Sets member scores from a snapshot, adding any new members at the given values.
    /// Unlike [`Self::apply_ops`], this tracks unknown members,
    /// since the snapshot might arrive before membership updates.
    ///
    /// Returns [`ScoreChange`]s, showing how each score changed
    /// (compared to `default_score` for new members).
    pub(crate) fn apply_snapshot(&mut self, snapshot: &ScoreSnapshot) -> Vec<ScoreChange> {
        let default = self.config.default_score;
        let mut changes = Vec::new();
        for (member_id, score) in &snapshot.diverged {
            let previous = self.scores.get(member_id).copied().unwrap_or(default);
            self.scores.insert(member_id.clone(), *score);
            if *score != previous {
                changes.push(ScoreChange {
                    member_id: member_id.clone(),
                    previous,
                    score: *score,
                });
            }
        }
        changes
    }

    /// Replace the score table with one read back from the store: the same
    /// table, not a change, so no [`ScoreChange`] is reported.
    pub(crate) fn restore_scores(&mut self, scores: Vec<(Vec<u8>, i64)>) {
        self.scores = scores.into_iter().collect();
    }

    /// Snapshot of scores that aren't set to the default, for ConversationSync.
    pub(crate) fn snapshot(&self) -> ScoreSnapshot {
        let default = self.config.default_score;
        let diverged = self
            .scores
            .iter()
            .filter(|(_, score)| **score != default)
            .map(|(id, score)| (id.clone(), *score))
            .collect();
        ScoreSnapshot { diverged }
    }

    // Returns the score for a given member, or None if they're not tracked.
    pub(crate) fn score_for(&self, member_id: &[u8]) -> Option<i64> {
        self.scores.get(member_id).copied()
    }

    /// Returns all tracked members and their scores (even those with the default score)
    pub(crate) fn all_members_with_scores(&self) -> Vec<(Vec<u8>, i64)> {
        self.scores.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Starting score for a newly tracked member.
    pub(crate) fn default_score(&self) -> i64 {
        self.config.default_score
    }

    /// Run [`ScoringConfig::validate`] over the config this service was built
    /// with, so construction rejects what an incoming sync would also reject.
    pub(crate) fn validate_config(&self) -> Result<(), ConversationError> {
        self.config.validate()
    }

    /// Updates the group's `default_peer_score`, changing members who still have the old default.
    ///
    /// This keeps everyone in sync after a ConversationSync.
    /// Members not listed in the sync snapshot are assumed to have the default,
    /// so we switch any local member at the old default to the new one.
    /// Members mentioned in the snapshot will get their scores updated right after,
    /// so this won't affect them.
    ///
    /// No [`ScoreChange`] is triggered here—no one's actual standing changed, just what "default" means.
    pub(crate) fn set_default_score(&mut self, default_score: i64) {
        let previous = self.config.default_score;
        self.config.default_score = default_score;
        if previous == default_score {
            return;
        }
        for score in self.scores.values_mut() {
            if *score == previous {
                *score = default_score;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::default_score_deltas;

    fn make_service() -> PeerScoringService {
        let deltas = HashMap::from([
            (ScoreEvent::EmergencyNoCreator, -50),
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::SuccessfulCommit, 10),
            (ScoreEvent::MisbehavingCommit, -30),
        ]);
        PeerScoringService::new(deltas, ScoringConfig { default_score: 100 })
    }

    fn op(member: &[u8], event: ScoreEvent) -> ScoreOp {
        ScoreOp {
            member_id: member.to_vec(),
            event,
        }
    }

    /// Apply a single op and return the change it reported, if any.
    fn apply_one(svc: &mut PeerScoringService, op: ScoreOp) -> Option<ScoreChange> {
        svc.apply_ops(&[op]).pop()
    }

    /// A batch that pushes a member down and then lifts it back reports where
    /// it *landed*, not where it passed through. Reporting the low-water mark
    /// would contradict `score_for` in the same breath, and an integrator
    /// acting on the event would act on a member who is fine.
    #[test]
    fn apply_ops_reports_the_net_result_not_the_low_water_mark() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        // 100 -50-> 50 -20-> 70: passes below 60, ends above it.
        let changes = svc.apply_ops(&[
            op(b"alice", ScoreEvent::BrokenCommit),
            op(b"alice", ScoreEvent::EmergencyYesCreator),
        ]);
        assert_eq!(
            changes,
            vec![ScoreChange {
                member_id: b"alice".to_vec(),
                previous: 100,
                score: 70,
            }]
        );
        assert_eq!(svc.score_for(b"alice"), Some(70), "event agrees with state");
    }

    /// Deltas that cancel leave the score where they found it, so there is
    /// nothing to report.
    #[test]
    fn apply_ops_is_silent_when_a_batch_nets_to_zero() {
        let mut svc = PeerScoringService::new(
            HashMap::from([
                (ScoreEvent::BrokenCommit, -50),
                (ScoreEvent::EmergencyNoCreator, 50),
            ]),
            ScoringConfig::default(),
        );
        svc.add_member(b"alice");
        let changes = svc.apply_ops(&[
            op(b"alice", ScoreEvent::BrokenCommit),
            op(b"alice", ScoreEvent::EmergencyNoCreator),
        ]);
        assert!(changes.is_empty());
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    /// One change per member however many ops touched them, in first-touch
    /// order so a batch reports deterministically.
    #[test]
    fn apply_ops_reports_once_per_member_in_first_touch_order() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.add_member(b"bob");
        let changes = svc.apply_ops(&[
            op(b"bob", ScoreEvent::BrokenCommit),
            op(b"alice", ScoreEvent::BrokenCommit),
            op(b"bob", ScoreEvent::BrokenCommit),
        ]);
        let reported: Vec<_> = changes
            .iter()
            .map(|c| (c.member_id.clone(), c.previous, c.score))
            .collect();
        assert_eq!(
            reported,
            vec![(b"bob".to_vec(), 100, 0), (b"alice".to_vec(), 100, 50),]
        );
    }

    /// An op against a member the table doesn't track is dropped, and a drop
    /// is not a change.
    #[test]
    fn untracked_member_reports_nothing() {
        let mut svc = make_service();
        assert!(apply_one(&mut svc, op(b"ghost", ScoreEvent::BrokenCommit)).is_none());
        assert_eq!(svc.score_for(b"ghost"), None);
    }

    /// Bootstrap measures an untracked member from `default_score`, so a
    /// joiner sees how far each member had already diverged from where it
    /// would otherwise have assumed they stood.
    #[test]
    fn apply_snapshot_reports_divergence_from_the_default() {
        let mut svc = make_service();
        let changes = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10), (b"bob".to_vec(), 40)],
        });
        assert_eq!(
            changes,
            vec![
                ScoreChange {
                    member_id: b"alice".to_vec(),
                    previous: 100,
                    score: -10,
                },
                ScoreChange {
                    member_id: b"bob".to_vec(),
                    previous: 100,
                    score: 40,
                },
            ]
        );
    }

    /// A re-sent sync carrying what the table already holds is not a change.
    #[test]
    fn apply_snapshot_is_silent_on_a_resend() {
        let mut svc = make_service();
        let snapshot = ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        };
        assert_eq!(svc.apply_snapshot(&snapshot).len(), 1);
        assert!(svc.apply_snapshot(&snapshot).is_empty());
    }

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
        apply_one(&mut svc, op(b"alice", ScoreEvent::EmergencyNoCreator));
        assert_eq!(svc.score_for(b"alice"), Some(50));
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
            apply_one(&mut svc, op(b"alice", event));
        }
        assert_eq!(svc.score_for(b"alice"), Some(30));
    }

    #[test]
    fn apply_ops_applies_every_op() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.add_member(b"bob");
        let ops = vec![
            op(b"alice", ScoreEvent::BrokenCommit),
            op(b"alice", ScoreEvent::BrokenCommit),
            op(b"bob", ScoreEvent::BrokenCommit),
            op(b"bob", ScoreEvent::BrokenCommit),
        ];
        svc.apply_ops(&ops);
        assert_eq!(svc.score_for(b"alice"), Some(0));
        assert_eq!(svc.score_for(b"bob"), Some(0));
    }

    #[test]
    fn snapshot_includes_only_diverged_scores() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.add_member(b"bob");
        svc.add_member(b"charlie");
        apply_one(&mut svc, op(b"alice", ScoreEvent::SuccessfulCommit));
        let snap = svc.snapshot();
        let ids: Vec<&[u8]> = snap.diverged.iter().map(|(id, _)| id.as_slice()).collect();
        assert_eq!(ids, vec![b"alice".as_slice()]);
        assert_eq!(snap.diverged[0].1, 110);
    }

    #[test]
    fn apply_snapshot_sets_absolute_scores() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.add_member(b"bob");
        svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10), (b"bob".to_vec(), 50)],
        });
        assert_eq!(svc.score_for(b"alice"), Some(-10));
        assert_eq!(svc.score_for(b"bob"), Some(50));
    }

    #[test]
    fn apply_snapshot_auto_tracks_unknown_member() {
        // A snapshot may arrive before the MLS membership view catches up,
        // so an unknown entry is tracked at the snapshot value.
        let mut svc = make_service();
        svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"newcomer".to_vec(), -10)],
        });
        assert_eq!(svc.score_for(b"newcomer"), Some(-10));
    }

    /// Adopting the group's default moves members this node had merely assumed
    /// were unremarkable. `peer_scores` omits members at the sender's default,
    /// so anything left on the old one is exactly a member the sender left out
    /// — the group's default is the right reading for them.
    #[test]
    fn adopting_a_default_rebases_members_left_on_the_old_one() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.add_member(b"bob");
        // bob diverges; alice stays on the assumed default.
        svc.apply_ops(&[op(b"bob", ScoreEvent::BrokenCommit)]);

        svc.set_default_score(80);

        assert_eq!(
            svc.score_for(b"alice"),
            Some(80),
            "rebased onto the group's"
        );
        assert_eq!(svc.score_for(b"bob"), Some(50), "a real score is untouched");
        assert_eq!(svc.default_score(), 80);
    }

    /// Adopting the same value changes nothing — a re-sent sync must not
    /// disturb the table.
    #[test]
    fn adopting_an_unchanged_default_is_a_noop() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        svc.set_default_score(100);
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    /// The whole point of adopting: after a sync, a member the sender omitted
    /// must end up on the sender's default, not on the one this node guessed.
    #[test]
    fn adopt_then_snapshot_reproduces_the_senders_table() {
        // Sender runs a default of 80.
        let mut sender = PeerScoringService::new(
            crate::default_score_deltas(),
            ScoringConfig { default_score: 80 },
        );
        sender.add_member(b"alice");
        sender.add_member(b"bob");
        sender.apply_ops(&[op(b"bob", ScoreEvent::BrokenCommit)]);
        let wire = sender.snapshot();
        assert!(
            !wire.diverged.iter().any(|(id, _)| id == b"alice"),
            "alice sits at the sender's default and is omitted"
        );

        // Joiner guessed 100, seeded both, then adopts and applies.
        let mut joiner = make_service();
        joiner.add_member(b"alice");
        joiner.add_member(b"bob");
        joiner.set_default_score(80);
        joiner.apply_snapshot(&wire);

        let mut sent = sender.all_members_with_scores();
        let mut got = joiner.all_members_with_scores();
        sent.sort();
        got.sort();
        assert_eq!(got, sent, "omitted member landed on the sender's default");
    }

    // ── Config validation ──────────────────────────────────────────

    #[test]
    fn validate_config_rejects_a_non_positive_default() {
        let svc =
            PeerScoringService::new(default_score_deltas(), ScoringConfig { default_score: 0 });
        assert!(svc.validate_config().is_err());
    }

    // ── ConversationSync round trip ────────────────────────────────

    /// The sync contract: what one member sends must rebuild the same table on
    /// another. The encoding is sparse — a member sitting at `default_score`
    /// is omitted — so the round trip has to survive that omission rather than
    /// dropping the member.
    #[test]
    fn snapshot_round_trips_through_apply_snapshot() {
        let mut sender = make_service();
        for id in [b"alice".as_slice(), b"bob", b"carol"] {
            sender.add_member(id);
        }
        sender.apply_ops(&[
            op(b"alice", ScoreEvent::BrokenCommit),
            op(b"carol", ScoreEvent::SuccessfulCommit),
        ]);
        // bob never moved, so the sparse snapshot must omit him.
        let wire = sender.snapshot();
        assert!(!wire.diverged.iter().any(|(id, _)| id == b"bob"));

        let mut joiner = make_service();
        for id in [b"alice".as_slice(), b"bob", b"carol"] {
            joiner.add_member(id);
        }
        joiner.apply_snapshot(&wire);

        let mut sent = sender.all_members_with_scores();
        let mut got = joiner.all_members_with_scores();
        sent.sort();
        got.sort();
        assert_eq!(got, sent, "joiner reproduces the sender's table");
    }

    /// Underflow is the untested half of the saturating arithmetic: a member
    /// already at the floor must not wrap around to a positive score. Driven
    /// through a snapshot so the config itself stays one `validate` accepts.
    #[test]
    fn score_saturates_no_underflow() {
        let mut svc = PeerScoringService::new(
            HashMap::from([(ScoreEvent::BrokenCommit, i64::MIN)]),
            ScoringConfig::default(),
        );
        svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), i64::MIN)],
        });
        apply_one(&mut svc, op(b"alice", ScoreEvent::BrokenCommit));
        assert_eq!(svc.score_for(b"alice"), Some(i64::MIN));
    }

    /// An empty batch touches nothing and reports nothing.
    #[test]
    fn empty_batch_is_a_noop() {
        let mut svc = make_service();
        svc.add_member(b"alice");
        assert!(svc.apply_ops(&[]).is_empty());
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    #[test]
    fn score_saturates_no_overflow() {
        let mut svc = PeerScoringService::new(
            HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)]),
            ScoringConfig {
                default_score: i64::MAX,
            },
        );
        svc.add_member(b"alice");
        apply_one(&mut svc, op(b"alice", ScoreEvent::SuccessfulCommit));
        assert_eq!(svc.score_for(b"alice"), Some(i64::MAX));
    }

    #[test]
    fn an_omitted_event_keeps_its_default_delta() {
        let mut svc = PeerScoringService::new(
            HashMap::from([(ScoreEvent::EmergencyNoCreator, -50)]),
            ScoringConfig::default(),
        );
        svc.add_member(b"alice");
        // Overridden entry takes the caller's value.
        apply_one(&mut svc, op(b"alice", ScoreEvent::EmergencyNoCreator));
        assert_eq!(svc.score_for(b"alice"), Some(50));
        // Omitted entry keeps the RFC default (+10), not 0.
        apply_one(&mut svc, op(b"alice", ScoreEvent::SuccessfulCommit));
        assert_eq!(svc.score_for(b"alice"), Some(60));
    }
}
