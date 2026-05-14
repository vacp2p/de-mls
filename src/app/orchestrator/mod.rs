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
use tokio::sync::{RwLock, broadcast};

use crate::{
    app::{SessionRunner, UserError},
    core::{
        ConsensusPlugin, ConversationConfig, ConversationLifecycle, ConversationPluginsFactory,
        DefaultConsensusPlugin, PeerScoringEvent, PluginConsensus, ScoringConfig, SessionEvent,
        StewardListConfig,
    },
    ds::{DeliveryService, OutboundPacket},
    identity::{Identity, WalletIdentity},
    mls_crypto::{KeyPackageBytes, MlsError},
};

/// Default capacity for the User's [`ConversationLifecycle`] broadcast
/// channel. Sized for batch login flows; subscribers that fall behind by
/// more than this lose events.
const CONVERSATION_LIFECYCLE_CAPACITY: usize = 128;

mod consensus;
mod consensus_events;
mod freeze;
mod inbound;
mod lifecycle;
mod messaging;
mod plugins;
mod query;
mod steward;

use plugins::UserPlugins;
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
type ConversationRegistry<P, CP> = Arc<RwLock<HashMap<String, Arc<RwLock<SessionRunner<P, CP>>>>>>;

pub struct User<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// User-level identity. Source of truth for "who am I"; per-conv MLS
    /// state lives on each `SessionRunner`.
    identity: Arc<dyn Identity>,
    /// `identity.identity_bytes()` materialised once at construction so every
    /// orchestrator method that needs the local identity bytes reads them
    /// without re-walking the `Identity` trait + allocating a fresh `Vec`.
    self_identity: Arc<[u8]>,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped. Cached
    /// as `Arc<[u8]>` so each `SessionRunner` cheaply shares it.
    app_id: Arc<[u8]>,
    /// Synchronous outbound transport. Cloned into each `SessionRunner` at
    /// construction; the User-level helper wraps [`DeliveryService::send`]
    /// in `spawn_blocking` for use in async contexts.
    transport: Arc<dyn DeliveryService>,
    /// All User-level plugin state: the per-conversation factory, the
    /// consensus context, the key-package provider, and the three default
    /// configs cloned into newly-created sessions.
    plugins: UserPlugins<P, CP>,
    /// Per-conversation `SessionRunner`s. Outer lock guards map CRUD;
    /// inner per-runner lock guards per-conversation reads/mutations so a
    /// write on conversation A doesn't block reads on conversation B.
    conversations: ConversationRegistry<P, CP>,
    /// User-level conversation lifecycle channel. Emits `Created(name)` /
    /// `Removed(name)` so integrators can subscribe to per-session
    /// [`SessionEvent`] streams as conversations come and go.
    lifecycle: broadcast::Sender<ConversationLifecycle>,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Clone for User<P, CP> {
    fn clone(&self) -> Self {
        Self {
            identity: Arc::clone(&self.identity),
            self_identity: Arc::clone(&self.self_identity),
            app_id: Arc::clone(&self.app_id),
            transport: Arc::clone(&self.transport),
            plugins: self.plugins.clone(),
            conversations: Arc::clone(&self.conversations),
            lifecycle: self.lifecycle.clone(),
        }
    }
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    fn new_with_plugins(
        identity: Arc<dyn Identity>,
        plugins: UserPlugins<P, CP>,
        transport: Arc<dyn DeliveryService>,
    ) -> Self {
        let self_identity: Arc<[u8]> = Arc::from(identity.identity_bytes());
        let (lifecycle, _initial_rx) = broadcast::channel(CONVERSATION_LIFECYCLE_CAPACITY);
        Self {
            identity,
            self_identity,
            app_id: Arc::from(uuid::Uuid::new_v4().as_bytes().as_slice()),
            transport,
            plugins,
            conversations: Arc::new(RwLock::new(HashMap::new())),
            lifecycle,
        }
    }

    /// Override the seed [`ScoringConfig`] used for newly-created per-conversation
    /// scoring plug-ins. Existing conversations are untouched; their plug-ins
    /// already own their live config (joiner-side overwritten by ConversationSync).
    pub fn set_default_scoring_config(&mut self, config: ScoringConfig) {
        self.plugins.default_scoring_config = config;
    }

    /// Override the seed [`StewardListConfig`] used for newly-created
    /// per-conversation steward-list plug-ins. Same lifecycle as
    /// [`Self::set_default_scoring_config`].
    pub fn set_default_steward_list_config(&mut self, config: StewardListConfig) {
        self.plugins.default_steward_list_config = config;
    }

    /// Look up a conversation runner. Returns `None` when no runner is
    /// registered for `conversation_name`. Takes the outer read lock briefly
    /// to clone the inner `Arc`, then releases it before the caller
    /// acquires the runner's own lock.
    pub async fn lookup_entry(
        &self,
        conversation_name: &str,
    ) -> Option<Arc<RwLock<SessionRunner<P, CP>>>> {
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
        f: impl FnOnce(&SessionRunner<P, CP>) -> R,
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

    /// Generate a single-use key package for our identity. Conversation-free —
    /// callable before `start_conversation`. Delegates to the configured
    /// [`crate::core::KeyPackageProvider`].
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        self.plugins.key_package_provider.generate()
    }

    /// Clone the conversation's consensus service handle. Cheap (the service
    /// is `Arc`-backed internally) and avoids holding the runner lock across
    /// `.await` points in consensus calls. Returns `None` when no runner is
    /// registered for `conversation_name`.
    pub(crate) async fn lookup_consensus(
        &self,
        conversation_name: &str,
    ) -> Option<PluginConsensus<P>> {
        let entry_arc = self.lookup_entry(conversation_name).await?;
        let entry = entry_arc.read().await;
        Some(entry.consensus.clone())
    }

    /// Subscribe to User-level conversation lifecycle events. Integrators
    /// use this to discover new sessions and subscribe to their per-session
    /// [`SessionEvent`] streams. Each call returns a fresh receiver.
    pub fn subscribe_conversations(&self) -> broadcast::Receiver<ConversationLifecycle> {
        self.lifecycle.subscribe()
    }

    /// Send an outbound packet via the User-level transport. Wraps the
    /// synchronous [`DeliveryService::send`] in `spawn_blocking` so the
    /// async context isn't blocked. Returns a transport message id (if
    /// any) on success.
    pub(crate) async fn send_outbound(&self, packet: OutboundPacket) -> Result<String, UserError> {
        let transport = Arc::clone(&self.transport);
        let send_result = tokio::task::spawn_blocking(move || transport.send(packet))
            .await
            .expect("transport send task panicked");
        Ok(send_result?)
    }

    /// Emit a [`SessionEvent`] on the named session's broadcast channel.
    /// No-op when no session is registered for `conversation_name`. The
    /// emit itself is fire-and-forget — subscribers that have fallen
    /// behind silently miss events.
    pub(crate) async fn emit(&self, conversation_name: &str, event: SessionEvent) {
        if let Some(entry_arc) = self.lookup_entry(conversation_name).await {
            entry_arc.read().await.emit_event(event);
        }
    }

    /// Build a fresh per-conversation `ConsensusService` via the User's
    /// `ConsensusContext`.
    fn build_consensus_service(&self) -> PluginConsensus<P> {
        self.plugins.consensus.build_service()
    }

    /// Drop this conversation's consensus scope from the shared storage and
    /// abort every auto-vote timer belonging to it. Called on leave and
    /// pending-join timeout.
    async fn cleanup_consensus_scope(&self, conversation_name: &str) -> Result<(), UserError> {
        self.cancel_conversation_auto_votes(conversation_name).await;
        let scope = P::Scope::from(conversation_name.to_string());
        self.plugins.consensus.delete_scope(&scope).await?;
        Ok(())
    }
}

// ─────────────────────────── DefaultConsensusPlugin Convenience ───────────────────────────

impl User<DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
    /// Construct a `User` on [`DefaultConsensusPlugin`] with the default config.
    pub fn with_private_key(
        private_key: &str,
        transport: Arc<dyn DeliveryService>,
    ) -> Result<Self, UserError> {
        Self::with_private_key_and_config(private_key, transport, ConversationConfig::default())
    }

    /// Construct a `User` on [`DefaultConsensusPlugin`] with a custom config.
    /// The user-level consensus storage + signer are derived from the private
    /// key; per-conversation `ConsensusService` instances are minted at
    /// conversation creation time, sharing the storage handle and cloning
    /// the signer.
    pub fn with_private_key_and_config(
        private_key: &str,
        transport: Arc<dyn DeliveryService>,
        default_conversation_config: ConversationConfig,
    ) -> Result<Self, UserError> {
        let private_key_signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = private_key_signer.address();

        let identity: Arc<dyn Identity> = Arc::new(WalletIdentity::from_wallet(user_address));
        let plugins = UserPlugins::<DefaultConsensusPlugin, _>::default_for_wallet(
            identity.as_ref(),
            private_key_signer,
            default_conversation_config,
        )?;

        Ok(Self::new_with_plugins(identity, plugins, transport))
    }
}
