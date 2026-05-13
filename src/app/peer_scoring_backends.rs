//! App-layer in-memory scoring storage. Plugs into the reference
//! [`PeerScoringService`](crate::core::PeerScoringService) in core.

use std::collections::HashMap;

use crate::core::PeerScoreStorage;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_storage_round_trip() {
        let mut storage = InMemoryPeerScoreStorage::new();
        assert_eq!(storage.get(b"alice"), None);
        storage.set(b"alice", 42);
        assert_eq!(storage.get(b"alice"), Some(42));
        storage.set(b"bob", -3);
        let all = storage.all_scores();
        assert_eq!(all.len(), 2);
        storage.remove(b"alice");
        assert_eq!(storage.get(b"alice"), None);
        assert_eq!(storage.all_scores().len(), 1);
    }
}
