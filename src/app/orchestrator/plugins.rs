//! [`ConversationPlugins`] trait — bundle of per-group plug-in types and the
//! construction methods for each. The trait is the single thing
//! integrators implement to swap any of the per-group plug-ins; the
//! convenience [`DefaultGroupPlugins`] supplies an in-memory build used
//! by the default-provider constructors.

use std::sync::Arc;

use crate::{
    app::{FixedScoringProvider, InMemoryPeerScoreStorage},
    core::{
        DeterministicStewardList, PeerScoringPlugin, PeerScoringService, ScoringConfig,
        StewardListConfig, StewardListPlugin,
    },
    mls_crypto::{
        KeyPackageBytes, MemoryDeMlsStorage, MlsCredentials, MlsError, MlsService, OpenMlsService,
    },
};

/// Per-group plug-in bundle. One trait carries the three plug-in types
/// (`Mls`, `Scoring`, `Steward`) plus the construction methods for each.
/// Identity is intentionally **not** part of this bundle — it lives
/// parallel to the group registry as `Arc<dyn Identity>` on `User`.
pub trait ConversationPlugins: Send + Sync + 'static {
    type Mls: MlsService;
    type Scoring: PeerScoringPlugin;
    type Steward: StewardListPlugin;

    /// Build an MLS service for a brand-new group as its sole creator.
    fn create_mls(&self, conversation_id: String) -> Result<Self::Mls, MlsError>;

    /// Try to build an MLS service from a serialized MLS welcome.
    /// Returns `Ok(None)` when the welcome isn't for us.
    fn welcome_mls(&self, welcome_bytes: &[u8]) -> Result<Option<Self::Mls>, MlsError>;

    /// Generate a single-use key package. Independent of any group's MLS
    /// service so a joiner can publish a key package before joining.
    fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError>;

    /// Build a fresh peer-scoring plug-in for a new group entry.
    fn make_scoring(&self, config: &ScoringConfig) -> Self::Scoring;

    /// Build a fresh steward-list plug-in for a new group entry.
    /// Returns an empty plug-in; lifecycle creator path bootstraps via
    /// [`StewardListPlugin::install_list`].
    fn make_steward(&self, conversation_id: &[u8], config: StewardListConfig) -> Self::Steward;
}

/// MLS service type for the default `DefaultProvider`-backed `User`. Uses
/// in-memory storage shared across every per-group service via `Arc` (the
/// `Arc<S>: DeMlsStorage` blanket impl makes this work).
pub type DefaultMlsService = OpenMlsService<Arc<MemoryDeMlsStorage>>;

/// Peer-scoring plug-in type for the default-config `User`: the
/// reference [`PeerScoringService`] over in-memory storage and a
/// fixed-table provider.
pub type DefaultPeerScoring = PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider>;

/// Steward-list plug-in type for the default-config `User`: the
/// reference [`DeterministicStewardList`].
pub type DefaultStewardList = DeterministicStewardList;

/// Default plug-in bundle used by the convenience
/// [`super::User::with_private_key`] constructor. In-memory MLS storage
/// shared across per-group services; reference scoring + steward
/// implementations.
pub struct DefaultGroupPlugins {
    pub(crate) storage: Arc<MemoryDeMlsStorage>,
    pub(crate) credentials: Arc<MlsCredentials>,
}

impl ConversationPlugins for DefaultGroupPlugins {
    type Mls = DefaultMlsService;
    type Scoring = DefaultPeerScoring;
    type Steward = DefaultStewardList;

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

    fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        OpenMlsService::<Arc<MemoryDeMlsStorage>>::generate_key_package(
            &self.storage,
            &self.credentials,
        )
    }

    fn make_scoring(&self, config: &ScoringConfig) -> Self::Scoring {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::with_default_deltas(),
            config.clone(),
        )
    }

    fn make_steward(&self, conversation_id: &[u8], config: StewardListConfig) -> Self::Steward {
        DeterministicStewardList::empty(conversation_id.to_vec(), config)
    }
}
