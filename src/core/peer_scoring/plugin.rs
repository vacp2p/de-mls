//! [`PeerScoringPlugin`] — the per-conversation scoring contract.
//!
//! Mutating methods return [`PeerScoringEvent`]s the coordinator drains
//! at safe points and turns into protocol actions. Storage backends and
//! event publishing are caller concerns.

use super::types::{PeerScoringEvent, ScoreOp, ScoreSnapshot};

/// Per-conversation peer-scoring plug-in. Mutating methods return any
/// [`PeerScoringEvent`]s the call produced; the coordinator drains them
/// at safe points and turns threshold crossings into protocol actions.
/// Storage backends and event publishing are caller concerns.
pub trait PeerScoringPlugin: Send + Sync + 'static {
    /// Start tracking a member at the plug-in's default score.
    ///
    /// MUST be called at most once per member: a duplicate call resets
    /// the score to default and discards any accumulated state. Callers
    /// guard with [`Self::score_for`] or a roster diff (see
    /// [`super::scoring_member_diff`]).
    ///
    /// Emits [`PeerScoringEvent::ThresholdCrossedDown`] iff the default
    /// score lands at-or-below threshold (unusual config); otherwise no
    /// event.
    fn add_member(&mut self, member_id: &[u8]) -> Vec<PeerScoringEvent>;

    /// Stop tracking a member. Emits no events — removal is a membership
    /// change, not a score event. Idempotent: a no-op on an untracked
    /// member.
    fn remove_member(&mut self, member_id: &[u8]);

    /// Apply a single [`ScoreOp`]. Untracked members are no-ops (no
    /// event, no auto-tracking — coordinators must call [`Self::add_member`]
    /// before applying ops). Returns events only when the op causes a
    /// threshold cross; repeated ops on a member already at-or-below
    /// threshold do not re-emit.
    fn apply_op(&mut self, op: &ScoreOp) -> Vec<PeerScoringEvent>;

    /// Apply a batch of [`ScoreOp`]s. Returns the concatenated events
    /// for every op in input order. A member crossing threshold twice
    /// in one batch (down then up, say) yields two events in that order.
    /// Default impl chains [`Self::apply_op`]; override for batched-write
    /// storage where the round-trip cost dominates.
    fn apply_ops(&mut self, ops: &[ScoreOp]) -> Vec<PeerScoringEvent> {
        ops.iter().flat_map(|op| self.apply_op(op)).collect()
    }

    /// Apply a snapshot of absolute scores (ConversationSync bootstrap).
    /// Unknown members are auto-tracked at the snapshot value — unlike
    /// [`Self::apply_op`], which ignores them — because a snapshot may
    /// arrive before the MLS membership view catches up.
    ///
    /// Emits a [`PeerScoringEvent`] per entry that crosses threshold;
    /// re-applying an unchanged snapshot emits nothing.
    fn apply_snapshot(&mut self, snapshot: &ScoreSnapshot) -> Vec<PeerScoringEvent>;

    /// Sparse snapshot of non-default scores for ConversationSync send.
    fn snapshot(&self) -> ScoreSnapshot;

    fn score_for(&self, member_id: &[u8]) -> Option<i64>;
    fn members_below_threshold(&self) -> Vec<Vec<u8>>;
    fn all_members_with_scores(&self) -> Vec<(Vec<u8>, i64)>;

    /// Current removal threshold. Coordinator reads this when building
    /// `ConversationSync` so joiners adopt the same value.
    fn threshold(&self) -> i64;

    /// Update the threshold in place. Emits NO events — even though a
    /// tightened threshold can re-classify members from above to
    /// at-or-below, the plug-in does not retroactively scan and emit.
    /// Event-driven coordinators MUST re-scan
    /// [`Self::members_below_threshold`] after a `set_threshold` call,
    /// or arrange for a subsequent apply that surfaces the affected
    /// members through normal cross-detection.
    fn set_threshold(&mut self, threshold: i64);

    /// Starting score for a newly tracked member. Called by
    /// [`Self::add_member`] and on `apply_snapshot` for unknown identities.
    fn default_score(&self) -> i64;

    /// Update the starting score in place. Applies to future
    /// [`Self::add_member`] calls; does not retroactively rescore
    /// already-tracked members.
    fn set_default_score(&mut self, score: i64);
}
