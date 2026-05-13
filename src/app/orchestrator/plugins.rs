//! [`DefaultConversationPluginsFactory`] — reference implementation of the
//! [`crate::core::ConversationPluginsFactory`] trait, used by the convenience
//! [`super::User::with_private_key`] constructor. In-memory MLS storage
//! shared across per-conversation services; reference scoring + steward
//! implementations.

use std::sync::Arc;

use crate::{
    app::InMemoryPeerScoreStorage,
    core::{
        ConversationPluginsFactory, DeterministicStewardList, PeerScoringService, ScoringConfig,
        StewardListConfig, default_score_deltas,
    },
    mls_crypto::{KeyPackageBytes, MemoryDeMlsStorage, MlsCredentials, MlsError, OpenMlsService},
};

/// MLS service type for the default `DefaultConsensusPlugin`-backed `User`. Uses
/// in-memory storage shared across every per-conversation service via
/// `Arc` (the `Arc<S>: DeMlsStorage` blanket impl makes this work).
pub type DefaultMlsService = OpenMlsService<Arc<MemoryDeMlsStorage>>;

/// Peer-scoring plug-in type for the default-config `User`: the
/// reference [`PeerScoringService`] over in-memory storage. The
/// per-event score deltas are supplied at construction (see
/// [`default_score_deltas`]).
pub type DefaultPeerScoring = PeerScoringService<InMemoryPeerScoreStorage>;

/// Steward-list plug-in type for the default-config `User`: the
/// reference [`DeterministicStewardList`].
pub type DefaultStewardList = DeterministicStewardList;

/// Default plug-in bundle used by the convenience
/// [`super::User::with_private_key`] constructor. In-memory MLS storage
/// shared across per-conversation services; reference scoring + steward
/// implementations.
pub struct DefaultConversationPluginsFactory {
    pub(crate) storage: Arc<MemoryDeMlsStorage>,
    pub(crate) credentials: Arc<MlsCredentials>,
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

    fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        OpenMlsService::<Arc<MemoryDeMlsStorage>>::generate_key_package(
            &self.storage,
            &self.credentials,
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
