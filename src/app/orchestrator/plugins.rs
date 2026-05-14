//! User-level plugin types and bundle.
//!
//! Two pieces:
//!
//! - [`UserPlugins`] — the bundle of all User-level plugin state: the
//!   per-conversation factory, the consensus context, the key package
//!   provider, and the three default configs cloned into new sessions.
//! - [`ConsensusContext`] — the consensus-plugin-side state (storage +
//!   signer) plus methods that build per-conv services and clean up
//!   scopes on leave.
//!
//! Also: [`DefaultConversationPluginsFactory`] — reference impl of
//! [`crate::core::ConversationPluginsFactory`] used by
//! [`super::User::with_private_key`].

use std::sync::Arc;

use hashgraph_like_consensus::storage::ConsensusStorage;

use crate::{
    app::{DefaultKeyPackageProvider, InMemoryPeerScoreStorage},
    core::{
        ConsensusPlugin, ConversationConfig, ConversationPluginsFactory, DeterministicStewardList,
        KeyPackageProvider, PeerScoringService, PluginConsensus, ScoringConfig, StewardListConfig,
        default_score_deltas,
    },
    mls_crypto::{MemoryDeMlsStorage, MlsCredentials, MlsError, OpenMlsService},
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

// ─────────────────────────── ConsensusContext ───────────────────────────

/// User-level consensus-plugin state: the shared storage handle + signer.
/// Held once on `User`; clones backing storage + signer into each new
/// per-conversation `ConsensusService`. Each per-conv service gets its own
/// private event bus (per upstream "service shape" recommendation).
pub struct ConsensusContext<P: ConsensusPlugin> {
    storage: P::ConsensusStorage,
    signer: P::Signer,
}

impl<P: ConsensusPlugin> ConsensusContext<P> {
    /// Build a fresh context. `P::new_storage` creates the shared backing
    /// once; `signer` is supplied by the integrator (typically wraps the
    /// user's wallet / account key).
    pub fn new(signer: P::Signer) -> Self {
        Self {
            storage: P::new_storage(),
            signer,
        }
    }

    /// Build a fresh per-conversation `ConsensusService`. Clones the shared
    /// storage handle so all per-conv services share one underlying
    /// persistence (scope-keyed); clones the signer; mints a fresh private
    /// event bus.
    pub fn build_service(&self) -> PluginConsensus<P> {
        PluginConsensus::<P>::new_with_components(
            self.storage.clone(),
            P::new_event_bus(),
            self.signer.clone(),
            10,
        )
    }

    /// Drop a conversation's scope from the shared consensus storage.
    /// Called on conversation leave.
    pub async fn delete_scope(
        &self,
        scope: &P::Scope,
    ) -> Result<(), hashgraph_like_consensus::error::ConsensusError> {
        self.storage.delete_scope(scope).await
    }

    /// Borrow the underlying consensus storage handle. Used by the User
    /// to fetch a proposal payload after a `ConsensusReached` event
    /// (`apply_consensus_outcome`), which is the only call site that
    /// reaches into storage outside the per-conv service.
    pub(crate) fn storage(&self) -> &P::ConsensusStorage {
        &self.storage
    }
}

impl<P: ConsensusPlugin> Clone for ConsensusContext<P> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            signer: self.signer.clone(),
        }
    }
}

// ─────────────────────────── UserPlugins ───────────────────────────

/// Bundle of all User-level plugin state. Held on `User` as a single
/// field so the User struct surfaces the registry + identity + transport
/// at top level and groups plugin concerns here.
pub struct UserPlugins<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Builds per-conversation plug-in instances (MLS service, scoring,
    /// steward list). One factory per `User`; instances live on the
    /// corresponding `SessionRunner` after creation.
    pub conversation_plugins: Arc<CP>,
    /// Consensus-plugin state. Owns the shared storage handle + signer
    /// and mints per-conv services on demand.
    pub consensus: ConsensusContext<P>,
    /// Identity-bound key package generator. Independent of any
    /// conversation — joiners can mint KPs before `start_conversation`.
    pub key_package_provider: Arc<dyn KeyPackageProvider>,
    /// Seed config copied into newly-created `SessionRunner`s.
    pub default_conversation_config: ConversationConfig,
    /// Seed config for the per-conversation peer-scoring plug-in.
    pub default_scoring_config: ScoringConfig,
    /// Seed config for the per-conversation steward-list plug-in.
    pub default_steward_list_config: StewardListConfig,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Clone for UserPlugins<P, CP> {
    fn clone(&self) -> Self {
        Self {
            conversation_plugins: Arc::clone(&self.conversation_plugins),
            consensus: self.consensus.clone(),
            key_package_provider: Arc::clone(&self.key_package_provider),
            default_conversation_config: self.default_conversation_config.clone(),
            default_scoring_config: self.default_scoring_config.clone(),
            default_steward_list_config: self.default_steward_list_config.clone(),
        }
    }
}

impl UserPlugins<crate::core::DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
    /// Build the default plug-in bundle from a wallet `PrivateKeySigner` and
    /// the derived identity. Used by [`super::User::with_private_key`].
    pub(crate) fn default_for_wallet(
        identity: &dyn crate::identity::Identity,
        signer: alloy::signers::local::PrivateKeySigner,
        default_conversation_config: ConversationConfig,
    ) -> Result<Self, MlsError> {
        let credentials = Arc::new(MlsCredentials::from_identity(identity)?);
        let storage = Arc::new(MemoryDeMlsStorage::new());
        let conversation_plugins = Arc::new(DefaultConversationPluginsFactory {
            storage: Arc::clone(&storage),
            credentials: Arc::clone(&credentials),
        });
        let key_package_provider: Arc<dyn KeyPackageProvider> =
            Arc::new(DefaultKeyPackageProvider {
                storage,
                credentials,
            });
        let consensus_signer =
            hashgraph_like_consensus::signing::EthereumConsensusSigner::new(signer);
        let consensus =
            ConsensusContext::<crate::core::DefaultConsensusPlugin>::new(consensus_signer);
        Ok(Self {
            conversation_plugins,
            consensus,
            key_package_provider,
            default_conversation_config,
            default_scoring_config: ScoringConfig::default(),
            default_steward_list_config: StewardListConfig::default(),
        })
    }
}
