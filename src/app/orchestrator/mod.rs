//! [`User`] — multi-conversation facade over core. One node owns one `User`, which
//! holds the consensus service, event handler, and per-conversation registry.
//! Each conversation's MLS service, peer-scoring plug-in, and steward-list
//! plug-in live on the [`SessionRunner`] (one instance per conversation),
//! which owns a [`crate::core::ConversationHandle`]. Methods split across
//! the submodules: `lifecycle` (create/leave), `query` (read-only getters),
//! `messaging` (send/ban), `consensus` (voting), `consensus_events`
//! (outcome dispatch), `inbound` (packet dispatch), `freeze` (timers),
//! `steward` (steward-side housekeeping).
//!
//! `User` is `Clone` — all fields are `Arc` or cheap `Clone` — so background
//! tasks just take their own handle via `self.clone()`.

use std::{collections::HashMap, str::FromStr, sync::Arc};

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::{signing::EthereumConsensusSigner, storage::ConsensusStorage};
use tokio::sync::RwLock;

use crate::{
    app::{SessionRunner, UserError},
    core::{
        ConsensusPlugin, ConversationConfig, ConversationEventHandler, ConversationPluginsFactory,
        DefaultConsensusPlugin, PeerScoringEvent, PluginConsensus, ScoringConfig,
        StewardListConfig,
    },
    identity::{Identity, WalletIdentity},
    mls_crypto::{KeyPackageBytes, MemoryDeMlsStorage, MlsCredentials, MlsError},
};

mod consensus;
mod consensus_events;
mod freeze;
mod inbound;
mod lifecycle;
mod messaging;
mod plugins;
mod query;
mod steward;

pub use plugins::{
    DefaultConversationPluginsFactory, DefaultMlsService, DefaultPeerScoring, DefaultStewardList,
};

/// `true` iff `events` contains at least one downward threshold cross —
/// the signal coordinators react to by chaining into score-removal
/// initiation. One helper so every callsite uses the same triggering rule.
pub(crate) fn has_downward_cross(events: &[PeerScoringEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, PeerScoringEvent::ThresholdCrossedDown { .. }))
}

/// Per-user registry of conversation runners, with one outer lock for map
/// CRUD and one inner lock per runner so writes on one conversation don't
/// block reads on another.
type ConversationRegistry<CP> = Arc<RwLock<HashMap<String, Arc<RwLock<SessionRunner<CP>>>>>>;

pub struct User<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// User-level identity, shared across every conversation this user is in.
    /// Source of truth for "who am I"; MLS state lives in each per-conversation
    /// service, and MLS credentials are captured by the plug-in bundle (built
    /// once from this identity at `User` init).
    identity: Arc<dyn Identity>,
    /// `identity.identity_bytes()` materialised once at construction so every
    /// orchestrator method that needs the local identity bytes reads them
    /// without re-walking the `Identity` trait + allocating a fresh `Vec`.
    self_identity: Arc<[u8]>,
    /// Builds per-conversation plug-in instances (MLS service, scoring,
    /// steward list) and the user-level key package on demand. One factory
    /// per `User`; the instances it mints live on the corresponding
    /// `SessionRunner` in `conversations` below.
    plugin_factory: Arc<CP>,
    /// Per-conversation `SessionRunner`s, each owning the plug-in instances
    /// built by `plugin_factory` at conversation creation. Outer lock guards
    /// map CRUD (insert / remove / iterate names); inner per-runner lock
    /// guards per-conversation reads and mutations, so a write on
    /// conversation A doesn't block reads on conversation B.
    conversations: ConversationRegistry<CP>,
    consensus_service: Arc<PluginConsensus<P>>,
    handler: Arc<dyn ConversationEventHandler>,
    default_conversation_config: ConversationConfig,
    /// Seed config for the per-conversation peer-scoring plug-in. Owned at
    /// the User level so every conversation starts from the same defaults;
    /// once a conversation exists, its plug-in owns the live values
    /// (joiner-side overwritten by `ConversationSync`).
    default_scoring_config: ScoringConfig,
    /// Seed config for the per-conversation steward-list plug-in. Same
    /// ownership story as `default_scoring_config`.
    default_steward_list_config: StewardListConfig,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped.
    app_id: Vec<u8>,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Clone for User<P, CP> {
    fn clone(&self) -> Self {
        Self {
            identity: Arc::clone(&self.identity),
            self_identity: Arc::clone(&self.self_identity),
            plugin_factory: Arc::clone(&self.plugin_factory),
            conversations: Arc::clone(&self.conversations),
            consensus_service: Arc::clone(&self.consensus_service),
            handler: Arc::clone(&self.handler),
            default_conversation_config: self.default_conversation_config.clone(),
            default_scoring_config: self.default_scoring_config.clone(),
            default_steward_list_config: self.default_steward_list_config.clone(),
            app_id: self.app_id.clone(),
        }
    }
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    fn new_with_config(
        identity: Arc<dyn Identity>,
        plugin_factory: Arc<CP>,
        consensus_service: Arc<PluginConsensus<P>>,
        handler: Arc<dyn ConversationEventHandler>,
        default_conversation_config: ConversationConfig,
    ) -> Self {
        let self_identity: Arc<[u8]> = Arc::from(identity.identity_bytes());
        Self {
            identity,
            self_identity,
            plugin_factory,
            conversations: Arc::new(RwLock::new(HashMap::new())),
            consensus_service,
            handler,
            default_conversation_config,
            default_scoring_config: ScoringConfig::default(),
            default_steward_list_config: StewardListConfig::default(),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
        }
    }

    /// Override the seed [`ScoringConfig`] used for newly-created per-conversation
    /// scoring plug-ins. Existing conversations are untouched; their plug-ins
    /// already own their live config (joiner-side overwritten by ConversationSync).
    pub fn set_default_scoring_config(&mut self, config: ScoringConfig) {
        self.default_scoring_config = config;
    }

    /// Override the seed [`StewardListConfig`] used for newly-created
    /// per-conversation steward-list plug-ins. Same lifecycle as
    /// [`Self::set_default_scoring_config`].
    pub fn set_default_steward_list_config(&mut self, config: StewardListConfig) {
        self.default_steward_list_config = config;
    }

    /// Look up a conversation runner. Returns `None` when no runner is
    /// registered for `conversation_name`. Takes the outer read lock briefly
    /// to clone the inner `Arc`, then releases it before the caller
    /// acquires the runner's own lock.
    pub(crate) async fn lookup_entry(
        &self,
        conversation_name: &str,
    ) -> Option<Arc<RwLock<SessionRunner<CP>>>> {
        self.conversations
            .read()
            .await
            .get(conversation_name)
            .cloned()
    }

    /// Run `f` under the runner's own read lock. Returns `None` if the
    /// runner isn't present.
    pub(crate) async fn with_entry<R>(
        &self,
        conversation_name: &str,
        f: impl FnOnce(&SessionRunner<CP>) -> R,
    ) -> Option<R> {
        let entry_arc = self.lookup_entry(conversation_name).await?;
        let entry = entry_arc.read().await;
        Some(f(&entry))
    }

    /// Borrow the local identity. Source of truth for "who am I" at the
    /// User level.
    pub(crate) fn identity(&self) -> &dyn Identity {
        self.identity.as_ref()
    }

    /// Borrow the local identity bytes. Cached at construction; cheap to call.
    pub(crate) fn self_identity(&self) -> &[u8] {
        &self.self_identity
    }

    /// Wallet address as checksummed hex.
    pub fn identity_string(&self) -> String {
        self.identity.identity_display().to_string()
    }

    /// Generate a single-use key package for our identity.
    pub(crate) fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        self.plugin_factory.generate_key_package()
    }

    /// Borrow the consensus event bus. Used by the gateway forwarder to
    /// subscribe for `ProposalDecided` notifications. Per-conversation
    /// consensus (Track A) will move this onto `ConversationHandle`.
    pub fn consensus_event_bus(&self) -> &P::EventBus {
        self.consensus_service.event_bus()
    }

    /// Drop all proposals / votes / sessions for this conversation from the
    /// consensus service and abort every auto-vote timer belonging to it.
    /// Called on leave, pending-join timeout, and re-creation.
    async fn cleanup_consensus_scope(&self, conversation_name: &str) -> Result<(), UserError> {
        self.cancel_conversation_auto_votes(conversation_name).await;
        let scope = P::Scope::from(conversation_name.to_string());
        self.consensus_service
            .storage()
            .delete_scope(&scope)
            .await?;
        Ok(())
    }
}

// ─────────────────────────── DefaultConsensusPlugin Convenience ───────────────────────────

impl User<DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
    /// Construct a `User` on [`DefaultConsensusPlugin`] with the default config.
    pub fn with_private_key(
        private_key: &str,
        handler: Arc<dyn ConversationEventHandler>,
    ) -> Result<Self, UserError> {
        Self::with_private_key_and_config(private_key, handler, ConversationConfig::default())
    }

    /// Construct a `User` on [`DefaultConsensusPlugin`] with a custom config.
    /// The user-level consensus service is built internally from the private
    /// key — `hashgraph_like_consensus 0.4.0` holds the signer inside the
    /// service, so the signer and the service are constructed together.
    pub fn with_private_key_and_config(
        private_key: &str,
        handler: Arc<dyn ConversationEventHandler>,
        default_conversation_config: ConversationConfig,
    ) -> Result<Self, UserError> {
        let private_key_signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = private_key_signer.address();

        let identity: Arc<dyn Identity> = Arc::new(WalletIdentity::from_wallet(user_address));
        let credentials = Arc::new(MlsCredentials::from_identity(identity.as_ref())?);
        let plugin_factory = Arc::new(DefaultConversationPluginsFactory {
            storage: Arc::new(MemoryDeMlsStorage::new()),
            credentials,
        });
        let consensus_signer = EthereumConsensusSigner::new(private_key_signer);
        let consensus_service = Arc::new(PluginConsensus::<DefaultConsensusPlugin>::new(
            consensus_signer,
        ));

        Ok(Self::new_with_config(
            identity,
            plugin_factory,
            consensus_service,
            handler,
            default_conversation_config,
        ))
    }
}
