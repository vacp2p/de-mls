//! Default implementations of the library's pluggable pieces.
//!
//! Contents:
//! - [`crate::defaults::InMemoryPeerScoreStorage`] — `HashMap`-backed
//!   peer-score storage backend.
//! - [`crate::defaults::DefaultConsensusPlugin`] — in-memory consensus
//!   backend over `hashgraph_like_consensus` types.
//! - [`crate::defaults::DefaultPeerScoring`] — alias for [`PeerScoringService`]
//!   over the in-memory store.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use hashgraph_like_consensus::{
    events::ConsensusEventBus, scope::ConsensusScope, signing::EthereumConsensusSigner,
    storage::InMemoryConsensusStorage, types::ConsensusEvent,
};

use crate::{ConsensusPlugin, PeerScoreStorage, PeerScoringService, SyncConsensusReceiver};

// ═══════════════════════════════════════════════════════════════════
// In-memory peer-score storage
// ═══════════════════════════════════════════════════════════════════

#[derive(Debug, Clone, Default)]
pub struct InMemoryPeerScoreStorage {
    scores: HashMap<Vec<u8>, i64>,
}

impl PeerScoreStorage for InMemoryPeerScoreStorage {
    type Error = std::convert::Infallible;

    fn get(&self, member_id: &[u8]) -> Result<Option<i64>, Self::Error> {
        Ok(self.scores.get(member_id).copied())
    }

    fn set(&mut self, member_id: &[u8], score: i64) -> Result<(), Self::Error> {
        self.scores.insert(member_id.to_vec(), score);
        Ok(())
    }

    fn remove(&mut self, member_id: &[u8]) -> Result<(), Self::Error> {
        self.scores.remove(member_id);
        Ok(())
    }

    fn all_scores(&self) -> Result<Vec<(Vec<u8>, i64)>, Self::Error> {
        Ok(self.scores.iter().map(|(k, v)| (k.clone(), *v)).collect())
    }
}

// ═══════════════════════════════════════════════════════════════════
// Sync consensus event bus
// ═══════════════════════════════════════════════════════════════════

/// Single-consumer FIFO event bus for [`ConsensusEvent`]s.
///
/// `publish` appends to the shared queue; `subscribe` hands back a
/// receiver that pops from it.
#[derive(Clone)]
pub struct SyncEventBus<Scope: ConsensusScope> {
    queue: Arc<Mutex<VecDeque<(Scope, ConsensusEvent)>>>,
}

impl<Scope: ConsensusScope> Default for SyncEventBus<Scope> {
    fn default() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

impl<Scope: ConsensusScope> ConsensusEventBus<Scope> for SyncEventBus<Scope> {
    type Receiver = SyncEventReceiver<Scope>;

    fn subscribe(&self) -> Self::Receiver {
        SyncEventReceiver {
            queue: Arc::clone(&self.queue),
        }
    }

    fn publish(&self, scope: Scope, event: ConsensusEvent) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back((scope, event));
        }
    }
}

/// Receiver paired with [`SyncEventBus`]. Drained via
/// [`SyncConsensusReceiver::try_recv`].
pub struct SyncEventReceiver<Scope: ConsensusScope> {
    queue: Arc<Mutex<VecDeque<(Scope, ConsensusEvent)>>>,
}

impl<Scope: ConsensusScope> SyncConsensusReceiver<Scope> for SyncEventReceiver<Scope> {
    fn try_recv(&mut self) -> Option<(Scope, ConsensusEvent)> {
        self.queue.lock().ok()?.pop_front()
    }
}

// ═══════════════════════════════════════════════════════════════════
// Default consensus plug-in
// ═══════════════════════════════════════════════════════════════════

pub struct DefaultConsensusPlugin;

impl ConsensusPlugin for DefaultConsensusPlugin {
    type Scope = String;
    type ConsensusStorage = InMemoryConsensusStorage<String>;
    type EventBus = SyncEventBus<String>;
    type Signer = EthereumConsensusSigner;

    fn new_storage() -> Self::ConsensusStorage {
        InMemoryConsensusStorage::new()
    }

    fn new_event_bus() -> Self::EventBus {
        SyncEventBus::default()
    }
}

// ═══════════════════════════════════════════════════════════════════
// Reference plug-in type aliases
// ═══════════════════════════════════════════════════════════════════

/// Reference peer-scoring plug-in: [`PeerScoringService`] over in-memory
/// storage. The per-event score deltas are supplied at construction (see
/// [`crate::default_score_deltas`]).
pub type DefaultPeerScoring = PeerScoringService<InMemoryPeerScoreStorage>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_storage_round_trip() {
        // `InMemoryPeerScoreStorage::Error` is `Infallible`, so unwrap.
        let mut storage = InMemoryPeerScoreStorage::default();
        assert_eq!(storage.get(b"alice").unwrap(), None);
        storage.set(b"alice", 42).unwrap();
        assert_eq!(storage.get(b"alice").unwrap(), Some(42));
        storage.set(b"bob", -3).unwrap();
        let all = storage.all_scores().unwrap();
        assert_eq!(all.len(), 2);
        storage.remove(b"alice").unwrap();
        assert_eq!(storage.get(b"alice").unwrap(), None);
        assert_eq!(storage.all_scores().unwrap().len(), 1);
    }
}
