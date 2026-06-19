//! Default implementations of the library's non-MLS plug-in traits.
//!
//! Contents:
//! - [`crate::defaults::InMemoryPeerScoreStorage`] — `HashMap`-backed
//!   peer-score storage.
//! - [`crate::defaults::DefaultConsensusPlugin`] — in-memory consensus
//!   backend over `hashgraph_like_consensus` types.
//! - [`crate::defaults::DefaultPeerScoring`],
//!   [`crate::defaults::DefaultStewardList`] — type aliases for the reference
//!   peer-scoring and steward-list plug-ins.
//!
//! The reference MLS engine (the OpenMLS provider, credentials, key-package
//! generation, and the plug-in factory that bundles them) lives in the
//! integrator, not here — the protocol crate names no concrete MLS backend.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use hashgraph_like_consensus::{
    events::ConsensusEventBus, scope::ConsensusScope, signing::EthereumConsensusSigner,
    storage::InMemoryConsensusStorage, types::ConsensusEvent,
};

use crate::{
    ConsensusPlugin, DeterministicStewardList, PeerScoreStorage, PeerScoringService,
    SyncConsensusReceiver,
};

// ═══════════════════════════════════════════════════════════════════
// In-memory peer-score storage
// ═══════════════════════════════════════════════════════════════════

/// `HashMap`-backed [`PeerScoreStorage`] for tests and simple deployments.
/// Production integrators supply a durable backend.
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

// ═══════════════════════════════════════════════════════════════════
// Sync consensus event bus
// ═══════════════════════════════════════════════════════════════════

/// Single-consumer FIFO event bus for [`ConsensusEvent`]s. The default
/// `EventBus` for [`DefaultConsensusPlugin`].
///
/// `publish` appends to the shared queue; `subscribe` hands back a
/// receiver that pops from it. Clones share the queue.
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

/// In-memory consensus backend for tests and simple deployments.
///
/// Implements [`crate::ConsensusPlugin`].
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

/// Reference steward-list plug-in: [`DeterministicStewardList`].
pub type DefaultStewardList = DeterministicStewardList;

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
