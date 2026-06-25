//! Default implementations of the library's pluggable pieces.
//!
//! Contents:
//! - [`crate::defaults::InMemoryPeerScoreStorage`] — `HashMap`-backed
//!   peer-score storage backend.
//! - [`crate::defaults::DefaultConsensusPlugin`] — in-memory consensus
//!   backend over `hashgraph_like_consensus` types.
//! - [`crate::defaults::DefaultPeerScoring`] — alias for [`PeerScoringService`]
//!   over the in-memory store.

use std::collections::HashMap;

use hashgraph_like_consensus::{
    scope::ScopeID, signing::EthereumConsensusSigner, storage::InMemoryConsensusStorage,
};

use crate::{ConsensusPlugin, PeerScoreStorage, PeerScoringService};

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
// Default consensus plug-in
// ═══════════════════════════════════════════════════════════════════

/// In-memory consensus backend: one scope-keyed store shared across the
/// conversations built from it, plus the Ethereum vote signer.
pub struct DefaultConsensusPlugin {
    storage: InMemoryConsensusStorage<ScopeID>,
    signer: EthereumConsensusSigner,
}

impl DefaultConsensusPlugin {
    /// Build a backend around `signer`, with a fresh in-memory store that the
    /// conversations created from it all share.
    pub fn new(signer: EthereumConsensusSigner) -> Self {
        Self {
            storage: InMemoryConsensusStorage::new(),
            signer,
        }
    }
}

impl ConsensusPlugin for DefaultConsensusPlugin {
    type ConsensusStorage = InMemoryConsensusStorage<ScopeID>;
    type Signer = EthereumConsensusSigner;

    fn storage(&self) -> Self::ConsensusStorage {
        self.storage.clone()
    }

    fn signer(&self) -> Self::Signer {
        self.signer.clone()
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
