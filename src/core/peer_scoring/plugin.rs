//! [`PeerScoringPlugin`] — the per-conversation scoring contract.
//!
//! Mutating methods return `true` when the call drove a member down across
//! the threshold; the coordinator re-scans [`PeerScoringPlugin::members_below_threshold`]
//! at a safe point to act. Storage backends are caller concerns.

use crate::core::{ScoreOp, ScoreSnapshot};

/// Per-conversation peer-scoring plug-in. Mutating methods return `true`
/// iff the call moved at least one member down to at-or-below threshold;
/// the coordinator re-scans for the actual targets. Storage backends are
/// caller concerns.
pub trait PeerScoringPlugin {
    /// Start tracking a member at the plug-in's default score. Returns
    /// `true` iff the default score lands at-or-below threshold (unusual
    /// config).
    ///
    /// MUST be called at most once per member: a duplicate call resets
    /// the score to default and discards any accumulated state. Callers
    /// guard with [`Self::score_for`] or a roster diff (see
    /// [`super::scoring_member_diff`]).
    fn add_member(&mut self, member_id: &[u8]) -> bool;

    /// Stop tracking a member. Idempotent: a no-op on an untracked member.
    fn remove_member(&mut self, member_id: &[u8]);

    /// Apply a single [`ScoreOp`]. Untracked members are no-ops (no
    /// auto-tracking — coordinators must call [`Self::add_member`] first).
    /// Returns `true` only when the op crosses the member down; repeated
    /// ops on a member already at-or-below threshold return `false`.
    fn apply_op(&mut self, op: &ScoreOp) -> bool;

    /// Apply a batch of [`ScoreOp`]s. Returns `true` iff any op in the
    /// batch crossed its member down. Default impl chains [`Self::apply_op`]
    /// (applying all ops); override for batched-write storage.
    fn apply_ops(&mut self, ops: &[ScoreOp]) -> bool {
        let mut crossed = false;
        for op in ops {
            crossed |= self.apply_op(op);
        }
        crossed
    }

    /// Apply a snapshot of absolute scores (ConversationSync bootstrap).
    /// Unknown members are auto-tracked at the snapshot value — unlike
    /// [`Self::apply_op`], which ignores them — because a snapshot may
    /// arrive before the MLS membership view catches up.
    ///
    /// Returns `true` iff any entry crossed its member down; re-applying an
    /// unchanged snapshot returns `false`.
    fn apply_snapshot(&mut self, snapshot: &ScoreSnapshot) -> bool;

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
}
