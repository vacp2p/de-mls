//! Reference implementations of the library's plug-in traits.
//!
//! Gathered in one place so the trait surface (`core`, `mls_crypto`,
//! `app`) is honestly the library's protocol contract and these are the
//! batteries that ship alongside it. Production integrators normally
//! swap one or more for their own implementations — durable storage,
//! a different identity / consensus signer, custom scoring — and pull
//! in only the defaults they actually want.
//!
//! Contents:
//! - [`crate::defaults::MemoryDeMlsStorage`] — in-memory MLS keystore.
//! - [`crate::defaults::InMemoryPeerScoreStorage`] — `HashMap`-backed
//!   peer-score storage.
//! - [`crate::defaults::DefaultConsensusPlugin`] — in-memory consensus
//!   backend over `hashgraph_like_consensus` types.
//! - [`crate::defaults::DefaultMlsService`],
//!   [`crate::defaults::DefaultPeerScoring`],
//!   [`crate::defaults::DefaultStewardList`] — type aliases for the
//!   default-bundle per-conversation plug-ins.
//! - [`crate::defaults::DefaultConversationPluginsFactory`] —
//!   `ConversationPluginsFactory` wired to the above.
//! - [`crate::defaults::DefaultKeyPackageProvider`] — `KeyPackageProvider`
//!   over the same storage + credentials handles.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use hashgraph_like_consensus::{
    events::BroadcastEventBus, signing::EthereumConsensusSigner, storage::InMemoryConsensusStorage,
};
use openmls_rust_crypto::MemoryStorage;

use crate::core::{
    ConsensusPlugin, ConversationPluginsFactory, DeterministicStewardList, KeyPackageProvider,
    PeerScoreStorage, PeerScoringService, ScoringConfig, StewardListConfig, default_score_deltas,
};
use crate::mls_crypto::{DeMlsStorage, KeyPackageBytes, MlsCredentials, MlsError, OpenMlsService};

// ═══════════════════════════════════════════════════════════════════
// In-memory storage backends
// ═══════════════════════════════════════════════════════════════════

/// In-memory MLS keystore for development and testing.
///
/// All data is lost on restart. Production callers supply a persistent
/// [`DeMlsStorage`] (e.g. SQLite-backed) instead.
#[derive(Default)]
pub struct MemoryDeMlsStorage {
    key_package_refs: RwLock<HashSet<Vec<u8>>>,
    mls: MemoryStorage,
}

impl MemoryDeMlsStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DeMlsStorage for MemoryDeMlsStorage {
    type MlsStorage = MemoryStorage;
    type StorageError = openmls_rust_crypto::MemoryStorageError;

    fn store_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), MlsError> {
        self.key_package_refs.write()?.insert(hash_ref.to_vec());
        Ok(())
    }

    fn is_our_key_package(&self, hash_ref: &[u8]) -> Result<bool, MlsError> {
        Ok(self.key_package_refs.read()?.contains(hash_ref))
    }

    fn remove_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), MlsError> {
        self.key_package_refs.write()?.remove(hash_ref);
        Ok(())
    }

    fn mls_storage(&self) -> &Self::MlsStorage {
        &self.mls
    }
}

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
// Default consensus plug-in
// ═══════════════════════════════════════════════════════════════════

/// In-memory consensus plug-in suitable for tests and simple deployments.
/// The [`ConsensusPlugin`] trait itself is defined in [`crate::core`].
pub struct DefaultConsensusPlugin;

impl ConsensusPlugin for DefaultConsensusPlugin {
    type Scope = String;
    type ConsensusStorage = InMemoryConsensusStorage<String>;
    type EventBus = BroadcastEventBus<String>;
    type Signer = EthereumConsensusSigner;

    fn new_storage() -> Self::ConsensusStorage {
        InMemoryConsensusStorage::new()
    }

    fn new_event_bus() -> Self::EventBus {
        BroadcastEventBus::default()
    }
}

// ═══════════════════════════════════════════════════════════════════
// Default per-conversation plug-in bundle
// ═══════════════════════════════════════════════════════════════════

/// MLS service type for the default-bundle `User`. Uses in-memory storage
/// shared across every per-conversation service via `Arc` (the
/// `Arc<S>: DeMlsStorage` blanket impl makes this work).
pub type DefaultMlsService = OpenMlsService<Arc<MemoryDeMlsStorage>>;

/// Peer-scoring plug-in type for the default-bundle `User`: the reference
/// [`PeerScoringService`] over in-memory storage. The per-event score
/// deltas are supplied at construction (see [`default_score_deltas`]).
pub type DefaultPeerScoring = PeerScoringService<InMemoryPeerScoreStorage>;

/// Steward-list plug-in type for the default-bundle `User`: the reference
/// [`DeterministicStewardList`].
pub type DefaultStewardList = DeterministicStewardList;

/// Default per-conversation plug-in bundle: in-memory MLS storage shared
/// across per-conversation services; reference scoring + steward
/// implementations.
pub struct DefaultConversationPluginsFactory {
    pub(crate) storage: Arc<MemoryDeMlsStorage>,
    pub(crate) credentials: Arc<MlsCredentials>,
}

impl DefaultConversationPluginsFactory {
    /// Build the default factory from an MLS storage handle and the
    /// User-level [`MlsCredentials`]. Both are cloned into every
    /// per-conversation MLS service this factory creates.
    pub fn new(storage: Arc<MemoryDeMlsStorage>, credentials: Arc<MlsCredentials>) -> Self {
        Self {
            storage,
            credentials,
        }
    }
}

impl ConversationPluginsFactory for DefaultConversationPluginsFactory {
    type Mls = DefaultMlsService;
    type Scoring = DefaultPeerScoring;
    type StewardList = DefaultStewardList;

    fn create_mls(&self, conversation_id: String) -> Result<Self::Mls, MlsError> {
        OpenMlsService::new_as_creator(
            conversation_id,
            Arc::clone(&self.storage),
            Arc::clone(&self.credentials),
        )
    }

    fn welcome_mls(&self, welcome_bytes: &[u8]) -> Result<Option<Self::Mls>, MlsError> {
        OpenMlsService::new_from_welcome(
            welcome_bytes,
            Arc::clone(&self.storage),
            Arc::clone(&self.credentials),
        )
    }

    fn make_scoring(&self, config: &ScoringConfig) -> Self::Scoring {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            default_score_deltas(),
            config.clone(),
        )
    }

    fn make_steward_list(
        &self,
        conversation_id: &[u8],
        config: StewardListConfig,
    ) -> Self::StewardList {
        DeterministicStewardList::empty(conversation_id.to_vec(), config)
    }
}

// ═══════════════════════════════════════════════════════════════════
// Default key-package provider
// ═══════════════════════════════════════════════════════════════════

/// Default reference implementation of [`KeyPackageProvider`] backed by
/// in-memory MLS storage + a User-level [`MlsCredentials`]. Generates a
/// fresh single-use key package on each `generate()` call.
pub struct DefaultKeyPackageProvider {
    pub(crate) storage: Arc<MemoryDeMlsStorage>,
    pub(crate) credentials: Arc<MlsCredentials>,
}

impl DefaultKeyPackageProvider {
    /// Build the default key-package provider from an MLS storage handle
    /// and the User-level [`MlsCredentials`]. Both are typically shared
    /// with [`DefaultConversationPluginsFactory`].
    pub fn new(storage: Arc<MemoryDeMlsStorage>, credentials: Arc<MlsCredentials>) -> Self {
        Self {
            storage,
            credentials,
        }
    }
}

impl KeyPackageProvider for DefaultKeyPackageProvider {
    fn generate(&self) -> Result<KeyPackageBytes, MlsError> {
        OpenMlsService::<Arc<MemoryDeMlsStorage>>::generate_key_package(
            &self.storage,
            &self.credentials,
        )
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
