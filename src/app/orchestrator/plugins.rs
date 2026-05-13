//! [`ConversationPlugins`] trait — bundle of per-conversation plug-in types
//! plus the construction methods for each. Integrators implement this one
//! trait to swap any of the per-conversation plug-ins; the convenience
//! [`DefaultConversationPlugins`] supplies an in-memory build used by the
//! default-provider constructors.

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

/// Per-conversation plug-in bundle. One trait carries the three plug-in
/// types (`Mls`, `Scoring`, `StewardList`) plus the construction methods
/// for each. Identity is intentionally **not** part of this bundle — it
/// lives parallel to the conversation registry as `Arc<dyn Identity>` on
/// `User`.
pub trait ConversationPlugins: Send + Sync + 'static {
    type Mls: MlsService;
    type Scoring: PeerScoringPlugin;
    type StewardList: StewardListPlugin;

    /// Build an MLS service for a brand-new conversation as its sole creator.
    fn create_mls(&self, conversation_id: String) -> Result<Self::Mls, MlsError>;

    /// Try to build an MLS service from a serialized MLS welcome.
    /// Returns `Ok(None)` when the welcome isn't for us.
    fn welcome_mls(&self, welcome_bytes: &[u8]) -> Result<Option<Self::Mls>, MlsError>;

    /// Generate a single-use key package. Independent of any
    /// conversation's MLS service so a joiner can publish a key package
    /// before joining.
    fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError>;

    /// Build a fresh peer-scoring plug-in for a new conversation runner.
    fn make_scoring(&self, config: &ScoringConfig) -> Self::Scoring;

    /// Build a fresh steward-list plug-in for a new conversation runner.
    /// Returns an empty plug-in; the lifecycle creator path bootstraps it
    /// via [`StewardListPlugin::install_list`].
    fn make_steward_list(
        &self,
        conversation_id: &[u8],
        config: StewardListConfig,
    ) -> Self::StewardList;
}

/// MLS service type for the default `DefaultProvider`-backed `User`. Uses
/// in-memory storage shared across every per-conversation service via
/// `Arc` (the `Arc<S>: DeMlsStorage` blanket impl makes this work).
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
/// shared across per-conversation services; reference scoring + steward
/// implementations.
pub struct DefaultConversationPlugins {
    pub(crate) storage: Arc<MemoryDeMlsStorage>,
    pub(crate) credentials: Arc<MlsCredentials>,
}

impl ConversationPlugins for DefaultConversationPlugins {
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

    fn make_steward_list(
        &self,
        conversation_id: &[u8],
        config: StewardListConfig,
    ) -> Self::StewardList {
        DeterministicStewardList::empty(conversation_id.to_vec(), config)
    }
}
