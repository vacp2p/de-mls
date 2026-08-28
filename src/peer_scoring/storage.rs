//! [`PeerScoreStorage`] — the per-conversation peer-score table backend.

/// Stores peer scores for a single conversation:
/// maps each `member_id` (as bytes) to their i64 score.
///
/// This is the only part of scoring you need to implement.
/// All the logic — changing scores, snapshots, and event reporting —
/// is handled by the library's [`crate::PeerScoringService`].
///
/// The crate provides an in-memory version out of the box;
/// you can use a database if needed.
///
/// Each conversation uses its own storage instance, so keys don't overlap across conversations.
///
/// All methods can fail, so persistent backends can report errors (like I/O errors).
/// The in-memory default can't fail and uses [`std::convert::Infallible`].
pub trait PeerScoreStorage {
    /// Backend I/O error. Use [`std::convert::Infallible`] for a backend that
    /// cannot fail.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Current score for `member_id`, or `None` if the member isn't tracked.
    fn get(&self, member_id: &[u8]) -> Result<Option<i64>, Self::Error>;

    /// Set `member_id`'s score—add if new, update if already tracked.
    fn set(&mut self, member_id: &[u8], score: i64) -> Result<(), Self::Error>;

    /// Remove `member_id` from tracking. Does nothing if not found.
    fn remove(&mut self, member_id: &[u8]) -> Result<(), Self::Error>;

    /// Returns all members and their scores. Should include everyone, order doesn't matter.
    fn all_scores(&self) -> Result<Vec<(Vec<u8>, i64)>, Self::Error>;
}
