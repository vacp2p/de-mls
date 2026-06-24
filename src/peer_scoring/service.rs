//! [`PeerScoringService`] — the library's per-conversation peer-score
//! tracker. It owns the RFC scoring logic (delta application, threshold
//! evaluation, snapshot bootstrap); the integrator injects only the
//! [`PeerScoreStorage`] backend and a [`ScoringConfig`] (deltas, threshold,
//! default).

use std::collections::HashMap;

use crate::{
    ConversationError, PeerScoreStorage, ScoreEvent, ScoreOp, ScoreSnapshot, ScoringConfig,
};

/// Lift a backend's `Self::Error` into [`ConversationError`].
fn storage_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> ConversationError {
    ConversationError::ScoreStorage(Box::new(e))
}

/// One per conversation: applies [`ScoreEvent`] deltas to each member and
/// answers threshold queries, over the `storage` backend.
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

    /// Start tracking a member at the configured default score.
    pub fn add_member(&mut self, member_id: &[u8]) -> Result<(), ConversationError> {
        let default = self.config.default_score;
        self.storage.set(member_id, default).map_err(storage_err)
    }

    /// Stop tracking a member. Idempotent on an untracked member.
    pub fn remove_member(&mut self, member_id: &[u8]) -> Result<(), ConversationError> {
        self.storage.remove(member_id).map_err(storage_err)
    }

    /// Apply an incremental delta to an already-tracked member. Unlike
    /// `add_member` / `apply_snapshot`, this never creates an entry — a
    /// stale op must not resurrect a removed member. The coordinator
    /// `add_member`s first; a drop means roster and scores are out of sync.
    pub fn apply_op(&mut self, op: &ScoreOp) -> Result<(), ConversationError> {
        let Some(current) = self.storage.get(&op.member_id).map_err(storage_err)? else {
            tracing::debug!(
                member = ?op.member_id,
                event = ?op.event,
                "score op dropped: member not tracked (add_member first)"
            );
            return Ok(());
        };
        let delta = self.score_delta(op.event);
        let new_score = current.saturating_add(delta);
        self.storage
            .set(&op.member_id, new_score)
            .map_err(storage_err)
    }

    /// Apply a batch of [`ScoreOp`]s.
    pub fn apply_ops(&mut self, ops: &[ScoreOp]) -> Result<(), ConversationError> {
        for op in ops {
            self.apply_op(op)?;
        }
        Ok(())
    }

    /// Apply a snapshot of absolute scores (ConversationSync bootstrap).
    /// Unknown members are auto-tracked at the snapshot value — unlike
    /// [`Self::apply_op`], which ignores them — because a snapshot may
    /// arrive before the MLS membership view catches up.
    pub fn apply_snapshot(&mut self, snapshot: &ScoreSnapshot) -> Result<(), ConversationError> {
        for (member_id, new_score) in &snapshot.diverged {
            self.storage
                .set(member_id, *new_score)
                .map_err(storage_err)?;
        }
        Ok(())
    }

    /// Sparse snapshot of non-default scores for ConversationSync send.
    pub fn snapshot(&self) -> Result<ScoreSnapshot, ConversationError> {
        let default = self.config.default_score;
        let diverged = self
            .storage
            .all_scores()
            .map_err(storage_err)?
            .into_iter()
            .filter(|(_, score)| *score != default)
            .collect();
        Ok(ScoreSnapshot { diverged })
    }

    pub fn score_for(&self, member_id: &[u8]) -> Result<Option<i64>, ConversationError> {
        self.storage.get(member_id).map_err(storage_err)
    }

    /// Members at or below the removal threshold (RFC §Peer Scoring MUST:
    /// the periodic threshold evaluation). The coordinator sweeps this
    /// after a score change and initiates `ViolationType::SCORE_BELOW_THRESHOLD` removals.
    pub fn members_below_threshold(&self) -> Result<Vec<Vec<u8>>, ConversationError> {
        let threshold = self.config.threshold;
        Ok(self
            .storage
            .all_scores()
            .map_err(storage_err)?
            .into_iter()
            .filter(|(_, score)| *score <= threshold)
            .map(|(id, _)| id)
            .collect())
    }

    /// Complete roster of tracked members with their scores (includes
    /// default-scored members). Used for roster diffing and UI reads.
    pub fn all_members_with_scores(&self) -> Result<Vec<(Vec<u8>, i64)>, ConversationError> {
        self.storage.all_scores().map_err(storage_err)
    }

    /// Current removal threshold. Coordinator reads this when building
    /// `ConversationSync` so joiners adopt the same value.
    pub fn threshold(&self) -> i64 {
        self.config.threshold
    }

    /// Update the threshold in place. The next [`Self::members_below_threshold`]
    /// sweep surfaces any newly re-classified member.
    pub fn set_threshold(&mut self, threshold: i64) {
        self.config.threshold = threshold;
    }

    /// Starting score for a newly tracked member.
    pub fn default_score(&self) -> i64 {
        self.config.default_score
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::defaults::InMemoryPeerScoreStorage;

    fn make_service() -> PeerScoringService<InMemoryPeerScoreStorage> {
        let deltas = HashMap::from([
            (ScoreEvent::EmergencyNoCreator, -50),
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::SuccessfulCommit, 10),
            (ScoreEvent::MisbehavingCommit, -30),
        ]);
        PeerScoringService::new(
            InMemoryPeerScoreStorage::default(),
            deltas,
            ScoringConfig {
                default_score: 100,
                threshold: 0,
            },
        )
    }

    type R = Result<(), ConversationError>;

    fn op(member: &[u8], event: ScoreEvent) -> ScoreOp {
        ScoreOp {
            member_id: member.to_vec(),
            event,
        }
    }

    #[test]
    fn add_member_gets_default_score() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        assert_eq!(svc.score_for(b"alice")?, Some(100));
        assert!(!svc.members_below_threshold()?.contains(&b"alice".to_vec()));
        Ok(())
    }

    #[test]
    fn add_member_below_threshold_lands_below() -> R {
        let mut svc = PeerScoringService::new(
            InMemoryPeerScoreStorage::default(),
            HashMap::new(),
            ScoringConfig {
                default_score: -10,
                threshold: 0,
            },
        );
        svc.add_member(b"alice")?;
        assert!(svc.members_below_threshold()?.contains(&b"alice".to_vec()));
        Ok(())
    }

    #[test]
    fn unknown_member_returns_none() -> R {
        let svc = make_service();
        assert_eq!(svc.score_for(b"unknown")?, None);
        Ok(())
    }

    #[test]
    fn remove_member_clears_score() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        svc.remove_member(b"alice")?;
        assert_eq!(svc.score_for(b"alice")?, None);
        Ok(())
    }

    #[test]
    fn apply_event_decreases_score() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        svc.apply_op(&op(b"alice", ScoreEvent::EmergencyNoCreator))?;
        assert_eq!(svc.score_for(b"alice")?, Some(50));
        Ok(())
    }

    #[test]
    fn apply_op_unknown_member_is_noop() -> R {
        let mut svc = make_service();
        svc.apply_op(&op(b"unknown", ScoreEvent::EmergencyNoCreator))?;
        assert_eq!(svc.score_for(b"unknown")?, None);
        Ok(())
    }

    #[test]
    fn multiple_events_accumulate() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        for event in [
            ScoreEvent::EmergencyNoCreator,
            ScoreEvent::MisbehavingCommit,
            ScoreEvent::SuccessfulCommit,
        ] {
            svc.apply_op(&op(b"alice", event))?;
        }
        assert_eq!(svc.score_for(b"alice")?, Some(30));
        Ok(())
    }

    #[test]
    fn apply_ops_applies_every_op() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        svc.add_member(b"bob")?;
        // Drop alice across threshold (-50 + -50 = 0 ≤ 0) and bob too.
        let ops = vec![
            op(b"alice", ScoreEvent::BrokenCommit),
            op(b"alice", ScoreEvent::BrokenCommit),
            op(b"bob", ScoreEvent::BrokenCommit),
            op(b"bob", ScoreEvent::BrokenCommit),
        ];
        svc.apply_ops(&ops)?;
        let below = svc.members_below_threshold()?;
        assert!(below.contains(&b"alice".to_vec()));
        assert!(below.contains(&b"bob".to_vec()));
        Ok(())
    }

    #[test]
    fn snapshot_includes_only_diverged_scores() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        svc.add_member(b"bob")?;
        svc.add_member(b"charlie")?;
        svc.apply_op(&op(b"alice", ScoreEvent::SuccessfulCommit))?;
        let snap = svc.snapshot()?;
        let ids: Vec<&[u8]> = snap.diverged.iter().map(|(id, _)| id.as_slice()).collect();
        assert_eq!(ids, vec![b"alice".as_slice()]);
        assert_eq!(snap.diverged[0].1, 110);
        Ok(())
    }

    #[test]
    fn apply_snapshot_sets_absolute_scores() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        svc.add_member(b"bob")?;
        svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10), (b"bob".to_vec(), 50)],
        })?;
        assert_eq!(svc.score_for(b"alice")?, Some(-10));
        assert_eq!(svc.score_for(b"bob")?, Some(50));
        let below = svc.members_below_threshold()?;
        assert!(below.contains(&b"alice".to_vec()));
        assert!(!below.contains(&b"bob".to_vec()));
        Ok(())
    }

    #[test]
    fn apply_snapshot_auto_tracks_unknown_member() -> R {
        // A snapshot may arrive before the MLS membership view catches up,
        // so an unknown entry is tracked at the snapshot value.
        let mut svc = make_service();
        svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"newcomer".to_vec(), -10)],
        })?;
        assert_eq!(svc.score_for(b"newcomer")?, Some(-10));
        assert!(
            svc.members_below_threshold()?
                .contains(&b"newcomer".to_vec())
        );
        Ok(())
    }

    #[test]
    fn members_below_threshold_filters_correctly() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        svc.add_member(b"bob")?;
        svc.add_member(b"charlie")?;
        for event in [ScoreEvent::EmergencyNoCreator, ScoreEvent::BrokenCommit] {
            svc.apply_op(&op(b"alice", event))?;
        }
        for _ in 0..2 {
            svc.apply_op(&op(b"charlie", ScoreEvent::EmergencyNoCreator))?;
        }
        let below = svc.members_below_threshold()?;
        assert!(below.contains(&b"alice".to_vec()));
        assert!(below.contains(&b"charlie".to_vec()));
        assert!(!below.contains(&b"bob".to_vec()));
        Ok(())
    }

    #[test]
    fn set_threshold_changes_below_threshold_set() -> R {
        let mut svc = make_service();
        svc.add_member(b"alice")?;
        // Apply via snapshot to set an absolute score without going
        // through the delta table.
        svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        })?;

        svc.set_threshold(-50);
        assert!(!svc.members_below_threshold()?.contains(&b"alice".to_vec()));

        svc.set_threshold(-5);
        assert!(svc.members_below_threshold()?.contains(&b"alice".to_vec()));
        Ok(())
    }

    #[test]
    fn score_saturates_no_overflow() -> R {
        let mut svc = PeerScoringService::new(
            InMemoryPeerScoreStorage::default(),
            HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)]),
            ScoringConfig {
                default_score: i64::MAX,
                threshold: 0,
            },
        );
        svc.add_member(b"alice")?;
        svc.apply_op(&op(b"alice", ScoreEvent::SuccessfulCommit))?;
        assert_eq!(svc.score_for(b"alice")?, Some(i64::MAX));
        Ok(())
    }

    #[test]
    fn unknown_event_yields_zero_delta() -> R {
        let mut svc = PeerScoringService::new(
            InMemoryPeerScoreStorage::default(),
            HashMap::from([(ScoreEvent::EmergencyNoCreator, -50)]),
            ScoringConfig {
                default_score: 100,
                threshold: 0,
            },
        );
        svc.add_member(b"alice")?;
        svc.apply_op(&op(b"alice", ScoreEvent::SuccessfulCommit))?;
        assert_eq!(svc.score_for(b"alice")?, Some(100));
        Ok(())
    }
}
