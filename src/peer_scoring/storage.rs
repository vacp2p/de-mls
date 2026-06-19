//! [`PeerScoreStorage`] — per-conversation per-member score persistence.
//! Concrete backends (in-memory, on-disk, etc.) live in the app layer.

/// Per-member score persistence for a single conversation. One storage
/// instance per conversation; the app layer ships an in-memory default impl.
pub trait PeerScoreStorage {
    fn get(&self, member_id: &[u8]) -> Option<i64>;
    fn set(&mut self, member_id: &[u8], score: i64);
    fn remove(&mut self, member_id: &[u8]);
    fn all_scores(&self) -> Vec<(Vec<u8>, i64)>;
}
