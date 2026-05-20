//! [`User`] struct definition, `Clone`, constructor, accessors, and the
//! consensus-context helpers shared across the User submodules
//! (`lifecycle`, `inbound`, `registry`).
//!
//! `User` is `Clone` — all fields are `Arc` or cheap `Clone` — so background
//! tasks just take their own handle via `self.clone()`.

use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, RwLock},
};

use alloy::signers::local::PrivateKeySigner;
use tokio::sync::broadcast;

use crate::{
    app::{SessionRunner, UserError},
    core::{
        ConsensusPlugin, ConversationConfig, ConversationLifecycle, ConversationPluginsFactory,
        DefaultConsensusPlugin, PluginConsensus, ScoringConfig, StewardListConfig,
    },
    ds::SharedDeliveryService,
    identity::{Identity, WalletIdentity},
    mls_crypto::{KeyPackageBytes, MlsError},
};

use super::plugins::{DefaultConversationPluginsFactory, UserPlugins};

/// Default capacity for the User's [`ConversationLifecycle`] broadcast
/// channel. Sized for batch login flows; subscribers that fall behind by
/// more than this lose events.
const CONVERSATION_LIFECYCLE_CAPACITY: usize = 128;

/// Single registry entry: one `Arc<RwLock<SessionRunner>>` per conversation.
/// Cloned out of the registry under the outer read lock, then locked
/// independently — writes on one conversation don't block reads on another.
pub type SessionEntry<P, CP> = Arc<RwLock<SessionRunner<P, CP>>>;

/// Per-user registry of conversation runners. The outer lock guards map
/// CRUD; the inner per-runner lock guards per-conversation reads/mutations
/// so a write on conversation A doesn't block reads on conversation B.
pub(crate) type ConversationRegistry<P, CP> = Arc<RwLock<HashMap<String, SessionEntry<P, CP>>>>;

pub struct User<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// `identity.identity_bytes()` materialised once at construction so every
    /// method that needs the local identity bytes reads them without
    /// re-walking the `Identity` trait + allocating a fresh `Vec`.
    pub(crate) self_identity: Arc<[u8]>,
    /// Cached display form of the local identity (e.g. checksummed `0x…`
    /// hex). Cloned into each `SessionRunner` so per-session methods
    /// (`send_app_message`) can tag the `sender` field without re-walking
    /// the `Identity` trait + allocating each call.
    pub(crate) identity_display: Arc<str>,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped. Cached
    /// as `Arc<[u8]>` so each `SessionRunner` cheaply shares it.
    pub(crate) app_id: Arc<[u8]>,
    /// Synchronous outbound transport. Cloned into each `SessionRunner` at
    /// construction; sessions wrap [`crate::ds::DeliveryService::publish`]
    /// in `spawn_blocking` for use in async contexts. Stored behind a
    /// `Mutex` because the trait takes `&mut self`.
    pub(crate) transport: SharedDeliveryService,
    /// All User-level plugin state: the per-conversation factory, the
    /// consensus context, the key-package provider, and the three default
    /// configs cloned into newly-created sessions.
    pub(crate) plugins: UserPlugins<P, CP>,
    /// Per-conversation `SessionRunner`s. Outer lock guards map CRUD;
    /// inner per-runner lock guards per-conversation reads/mutations so a
    /// write on conversation A doesn't block reads on conversation B.
    pub(crate) conversations: ConversationRegistry<P, CP>,
    /// User-level conversation lifecycle channel. Emits `Created(name)` /
    /// `Removed(name)` so integrators can subscribe to per-session
    /// [`crate::core::SessionEvent`] streams as conversations come and go.
    pub(crate) lifecycle: broadcast::Sender<ConversationLifecycle>,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Clone for User<P, CP> {
    fn clone(&self) -> Self {
        Self {
            self_identity: Arc::clone(&self.self_identity),
            identity_display: Arc::clone(&self.identity_display),
            app_id: Arc::clone(&self.app_id),
            transport: Arc::clone(&self.transport),
            plugins: self.plugins.clone(),
            conversations: Arc::clone(&self.conversations),
            lifecycle: self.lifecycle.clone(),
        }
    }
}

// ── Public API ──────────────────────────────────────────────────────────

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Wallet address as checksummed hex.
    pub fn identity_string(&self) -> String {
        self.identity_display.to_string()
    }

    /// Generate a single-use key package for our identity. Conversation-free —
    /// callable before `start_conversation`. Delegates to the configured
    /// [`crate::core::KeyPackageProvider`].
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        self.plugins.key_package_provider.generate()
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
    /// Borrow the local identity bytes. Cached at construction; cheap to call.
    pub(crate) fn self_identity(&self) -> &[u8] {
        &self.self_identity
    }

    /// Build a fresh per-conversation `ConsensusService` via the User's
    /// `ConsensusContext`.
    pub(crate) fn build_consensus_service(&self) -> PluginConsensus<P> {
        self.plugins.consensus.build_service()
    }

    /// Drop this conversation's consensus scope from the shared storage and
    /// abort every auto-vote timer belonging to it. Called on leave and
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

    /// Build a `User` from a derived `Identity` + plug-in bundle. The
    /// identity is consumed only for its bytes + display form; nothing
    /// retains the trait object after construction.
    fn new_with_plugins(
        identity: &dyn Identity,
        plugins: UserPlugins<P, CP>,
        transport: SharedDeliveryService,
    ) -> Self {
        let self_identity: Arc<[u8]> = Arc::from(identity.identity_bytes());
        let identity_display: Arc<str> = Arc::from(identity.identity_display());
        let (lifecycle, _initial_rx) = broadcast::channel(CONVERSATION_LIFECYCLE_CAPACITY);
        Self {
            self_identity,
            identity_display,
            app_id: Arc::from(uuid::Uuid::new_v4().as_bytes().as_slice()),
            transport,
            plugins,
            conversations: Arc::new(RwLock::new(HashMap::new())),
            lifecycle,
        }
    }
}

// ── DefaultConsensusPlugin Convenience ──────────────────────────────────

impl User<DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
    /// Construct a `User` on [`DefaultConsensusPlugin`] with the default config.
    pub fn with_private_key(
        private_key: &str,
        transport: SharedDeliveryService,
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
        transport: SharedDeliveryService,
        default_conversation_config: ConversationConfig,
    ) -> Result<Self, UserError> {
        let private_key_signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = private_key_signer.address();

        let identity = WalletIdentity::from_wallet(user_address);
        let plugins = UserPlugins::<DefaultConsensusPlugin, _>::default_for_wallet(
            &identity,
            private_key_signer,
            default_conversation_config,
        )?;

        Ok(Self::new_with_plugins(&identity, plugins, transport))
    }
}
