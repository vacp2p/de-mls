//! [`User`] struct definition, constructor, accessors, and the
//! consensus-context helpers shared across the User submodules
//! (`lifecycle`, `inbound`, `registry`).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

use crate::{
    app::{SessionRunner, UserError, UserPlugins},
    core::{
        ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory, PluginConsensus,
        ScoringConfig, StewardListConfig,
    },
    ds::SharedDeliveryService,
    identity::Identity,
    mls_crypto::{KeyPackageBytes, MlsError},
};

/// Single registry entry: one `Arc<RwLock<SessionRunner>>` per conversation.
/// Cloned out of the registry under the outer read lock, then locked
/// independently — writes on one conversation don't block reads on another.
pub type SessionEntry<P, CP> = Arc<RwLock<SessionRunner<P, CP>>>;

/// Per-user registry of conversation runners. Each entry's inner per-runner
/// lock guards per-conversation reads/mutations so a write on conversation
/// A doesn't block reads on conversation B.
pub(crate) type ConversationRegistry<P, CP> = RwLock<HashMap<String, SessionEntry<P, CP>>>;

pub struct User<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Local identity, set once at construction and read-only thereafter.
    /// Accessed via the [`Identity`] trait wherever bytes or display
    /// form are needed; per-session cached `Arc<[u8]>` + `Arc<str>` are
    /// extracted from it at [`SessionRunner`] construction in
    /// [`super::lifecycle`].
    pub(crate) identity: Box<dyn Identity>,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped.
    pub(crate) app_id: Vec<u8>,
    /// Synchronous outbound transport. Cloned into each `SessionRunner` at
    /// construction. Stored behind a `Mutex` because the trait takes
    /// `&mut self`.
    pub(crate) transport: SharedDeliveryService,
    /// All User-level plugin state: the per-conversation factory, the
    /// consensus context, the key-package provider, and the three default
    /// configs cloned into newly-created sessions.
    pub(crate) plugins: UserPlugins<P, CP>,
    /// Per-conversation `SessionRunner`s.
    pub(crate) conversations: ConversationRegistry<P, CP>,
    /// User-level conversation lifecycle events: `Created(name)` /
    /// `Removed(name)`. Integrators drain via
    /// [`Self::drain_lifecycle_events`] once per polling cycle to learn
    /// when new sessions appear and old ones disappear. Interior `Mutex`
    /// so producer-side methods stay `&self`.
    pub(crate) pending_lifecycle_events: Mutex<Vec<ConversationLifecycle>>,
}

// ── Public API ──────────────────────────────────────────────────────────

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Display form of the local identity, derived from
    /// [`Identity::identity_display`]. Stable for the lifetime of the
    /// `User`; intended for logs and UI.
    pub fn identity_string(&self) -> String {
        self.identity.identity_display().to_string()
    }

    /// Generate a single-use key package for our identity. Conversation-free —
    /// callable before `start_conversation`. Delegates to the configured
    /// [`crate::core::ConversationPluginsFactory::generate_key_package`].
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        self.plugins.conversation_plugins.generate_key_package()
    }

    /// Drain every pending [`ConversationLifecycle`] event accumulated
    /// since the last call. Returns events in insertion order. Callers
    /// (gateway, integrator) invoke this once per polling cycle to discover
    /// `Created` / `Removed` sessions and wire up per-session event
    /// drains via [`SessionRunner::drain_events`].
    pub fn drain_lifecycle_events(&self) -> Vec<ConversationLifecycle> {
        match self.pending_lifecycle_events.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
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
}

// ── User-internal helpers ───────────────────────────────────────────────

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Borrow the local identity bytes via the [`Identity`] trait.
    pub(crate) fn self_identity(&self) -> &[u8] {
        self.identity.identity_bytes()
    }

    /// Append a [`ConversationLifecycle`] event to the pending-events buffer
    /// for [`Self::drain_lifecycle_events`]. Silent on poison —
    /// emit is fire-and-forget.
    pub(crate) fn emit_lifecycle(&self, event: ConversationLifecycle) {
        if let Ok(mut buf) = self.pending_lifecycle_events.lock() {
            buf.push(event);
        }
    }

    /// Build a fresh per-conversation `ConsensusService` via the User's
    /// `ConsensusContext`.
    pub(crate) fn build_consensus_service(&self) -> PluginConsensus<P> {
        self.plugins.consensus.build_service()
    }

    /// Drop this conversation's consensus scope from the shared storage and
    /// clear every auto-vote registered for it. Called on leave and
    /// pending-join timeout.
    pub(crate) async fn cleanup_consensus_scope(
        &self,
        conversation_name: &str,
    ) -> Result<(), UserError> {
        if let Some(entry_arc) = self.lookup_entry(conversation_name)? {
            entry_arc
                .write()
                .map_err(|_| UserError::LockPoisoned("session"))?
                .cancel_all_auto_votes();
        }
        let scope = P::Scope::from(conversation_name.to_string());
        self.plugins.consensus.delete_scope(&scope).await?;
        Ok(())
    }

    pub fn new_with_plugins(
        identity: Box<dyn Identity>,
        plugins: UserPlugins<P, CP>,
        transport: SharedDeliveryService,
    ) -> Self {
        Self {
            identity,
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            transport,
            plugins,
            conversations: RwLock::new(HashMap::new()),
            pending_lifecycle_events: Mutex::new(Vec::new()),
        }
    }
}
