//! [`PeerScoreStorage`] — the per-conversation peer-score table backend.

/// Persistence for one conversation's peer-score table: a map from opaque
/// `member_id` bytes to an `i64` score.
///
/// This is the **only** scoring component an integrator implements. All
/// scoring behavior — turning events into deltas, threshold evaluation,
/// snapshot bootstrap — lives in the library-owned
/// [`crate::PeerScoringService`], which drives this backend. An
/// implementation therefore needs no protocol knowledge; it just stores and
/// retrieves scores. The library ships [`crate::defaults::InMemoryPeerScoreStorage`]
/// as a ready default; a durable integrator might back it with sqlite, a
/// key-value store, etc.
///
/// One instance per conversation. The library never shares a backend across
/// conversations, so keys never collide between conversations.
///
/// Every method is fallible so durable backends can surface I/O errors
/// instead of swallowing them — a lost score write corrupts the table the
/// threshold-removal path depends on. The in-memory default uses
/// [`std::convert::Infallible`].
pub trait PeerScoreStorage {
    /// Backend I/O error. Use [`std::convert::Infallible`] for a backend that
    /// cannot fail.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Current score for `member_id`, or `None` if the member isn't tracked.
    fn get(&self, member_id: &[u8]) -> Result<Option<i64>, Self::Error>;

    /// Insert or overwrite `member_id`'s score (an upsert): start tracking a
    /// new member or replace an existing score. Must not fail on a key that
    /// already exists.
    fn set(&mut self, member_id: &[u8], score: i64) -> Result<(), Self::Error>;

    /// Stop tracking `member_id`. Idempotent — a no-op when the member isn't
    /// tracked.
    fn remove(&mut self, member_id: &[u8]) -> Result<(), Self::Error>;

    /// Every tracked member paired with its score. Must be **complete** — the
    /// library derives the below-threshold set and the scoring/roster diff
    /// from this — but order is irrelevant.
    fn all_scores(&self) -> Result<Vec<(Vec<u8>, i64)>, Self::Error>;
}
