//! [`User`] — multi-group facade over core. One node owns one `User`, which
//! holds the consensus service, event handler, and per-group state map.
//! Each group's MLS service and peer-scoring service live on the
//! [`SessionRunner`] (one instance per group), which in turn owns a
//! [`crate::core::GroupHandle`]. Methods split across the
//! submodules: `lifecycle` (create/leave), `query` (read-only getters),
//! `messaging` (send/ban), `consensus` (voting), `consensus_events`
//! (outcome dispatch), `inbound` (packet dispatch), `freeze` (timers),
//! `steward` (steward-side housekeeping).
//!
//! `User` is `Clone` — all fields are `Arc` or cheap `Clone` — so background
//! tasks just take their own handle via `self.clone()`.

use std::{collections::HashMap, str::FromStr, sync::Arc};

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::storage::ConsensusStorage;
use tokio::sync::RwLock;

use crate::{
    app::{SessionRunner, UserError},
    core::{
        DeMlsProvider, DefaultProvider, GroupConfig, GroupEventHandler, PeerScoringEvent,
        ProviderConsensus, ScoringConfig, StewardListConfig,
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
    DefaultGroupPlugins, DefaultMlsService, DefaultPeerScoring, DefaultStewardList, GroupPlugins,
};

/// `true` iff `events` contains at least one downward threshold cross —
/// the signal coordinators react to by chaining into score-removal
/// initiation. Helper kept here so every callsite uses the same
/// triggering rule.
pub(crate) fn has_downward_cross(events: &[PeerScoringEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, PeerScoringEvent::ThresholdCrossedDown { .. }))
}

/// Per-user registry of group runners, with one outer lock for map CRUD
/// and one inner lock per runner so writes on one group don't block reads
/// on another.
type GroupRegistry<M, Sc, St> = Arc<RwLock<HashMap<String, Arc<RwLock<SessionRunner<M, Sc, St>>>>>>;

pub struct User<P: DeMlsProvider, GP: GroupPlugins, H: GroupEventHandler> {
    /// Local user-level identity, shared across all this user's groups.
    /// Source of truth for "who am I"; MLS state lives in the per-group
    /// service, MLS credentials are captured by the plug-in bundle (built
    /// once from this identity at User init).
    identity: Arc<dyn Identity>,
    /// Per-user plug-in bundle: builds MLS services + key packages, plus
    /// per-group scoring and steward-list plug-ins on demand.
    plugins: Arc<GP>,
    /// Outer lock: map CRUD (insert / remove / iterate names).
    /// Inner per-entry lock: per-group reads and mutations. A write on
    /// group A doesn't block reads on group B. Per-group peer scoring
    /// lives inside the entry, so scoring access is guarded by the same
    /// `RwLock` — no separate scoring lock.
    groups: GroupRegistry<GP::Mls, GP::Scoring, GP::Steward>,
    consensus_service: Arc<ProviderConsensus<P>>,
    eth_signer: PrivateKeySigner,
    handler: Arc<H>,
    default_group_config: GroupConfig,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped.
    app_id: Vec<u8>,
}

impl<P: DeMlsProvider, GP: GroupPlugins, H: GroupEventHandler> Clone for User<P, GP, H> {
    fn clone(&self) -> Self {
        Self {
            identity: Arc::clone(&self.identity),
            plugins: Arc::clone(&self.plugins),
            groups: Arc::clone(&self.groups),
            consensus_service: Arc::clone(&self.consensus_service),
            eth_signer: self.eth_signer.clone(),
            handler: Arc::clone(&self.handler),
            default_group_config: self.default_group_config.clone(),
            app_id: self.app_id.clone(),
        }
    }
}

impl<P: DeMlsProvider, GP: GroupPlugins, H: GroupEventHandler + 'static> User<P, GP, H> {
    fn new_with_config(
        identity: Arc<dyn Identity>,
        plugins: Arc<GP>,
        consensus_service: Arc<ProviderConsensus<P>>,
        eth_signer: PrivateKeySigner,
        handler: Arc<H>,
        default_group_config: GroupConfig,
    ) -> Self {
        Self {
            identity,
            plugins,
            groups: Arc::new(RwLock::new(HashMap::new())),
            consensus_service,
            eth_signer,
            handler,
            default_group_config,
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
        }
    }

    /// Build a fresh per-group scoring plug-in from the user's default
    /// scoring config. Used by entry construction in lifecycle / welcome
    /// paths.
    pub(crate) fn make_scoring_service(&self) -> GP::Scoring {
        let scoring_config = ScoringConfig {
            default_score: self.default_group_config.default_peer_score,
            threshold: self.default_group_config.threshold_peer_score,
        };
        self.plugins.make_scoring(&scoring_config)
    }

    /// Build a fresh per-group steward-list plug-in. Returns an empty
    /// plug-in; lifecycle creator path bootstraps via `install_list`,
    /// joiner path leaves it empty until `GroupSync` arrives.
    pub(crate) fn make_steward_plugin(
        &self,
        group_name: &str,
        config: &StewardListConfig,
    ) -> GP::Steward {
        self.plugins
            .make_steward(group_name.as_bytes(), config.clone())
    }

    /// Look up a group runner. Returns `None` when the entry isn't present.
    /// Takes the outer read lock briefly to clone the inner `Arc`, then
    /// releases it before the caller acquires the runner's own lock.
    pub(crate) async fn lookup_entry(
        &self,
        group_name: &str,
    ) -> Option<Arc<RwLock<SessionRunner<GP::Mls, GP::Scoring, GP::Steward>>>> {
        self.groups.read().await.get(group_name).cloned()
    }

    /// Run `f` under the runner's own read lock. Returns `None` if the
    /// runner isn't present.
    pub(crate) async fn with_entry<R>(
        &self,
        group_name: &str,
        f: impl FnOnce(&SessionRunner<GP::Mls, GP::Scoring, GP::Steward>) -> R,
    ) -> Option<R> {
        let entry_arc = self.lookup_entry(group_name).await?;
        let entry = entry_arc.read().await;
        Some(f(&entry))
    }

    /// Borrow the local identity. Source of truth for "who am I" at the
    /// User level.
    pub(crate) fn identity(&self) -> &dyn Identity {
        self.identity.as_ref()
    }

    /// Wallet address as checksummed hex.
    pub fn identity_string(&self) -> String {
        self.identity.identity_display().to_string()
    }

    /// Generate a single-use key package for our identity.
    pub(crate) fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        self.plugins.generate_key_package()
    }

    /// Drop all proposals / votes / sessions for this group from the
    /// consensus service and abort every auto-vote timer belonging to it.
    /// Called on leave, pending-join timeout, and re-creation.
    async fn cleanup_consensus_scope(&self, group_name: &str) -> Result<(), UserError> {
        self.cancel_group_auto_votes(group_name).await;
        let scope = P::Scope::from(group_name.to_string());
        self.consensus_service
            .storage()
            .delete_scope(&scope)
            .await?;
        Ok(())
    }
}

// ─────────────────────────── DefaultProvider Convenience ───────────────────────────

impl<H: GroupEventHandler + 'static> User<DefaultProvider, DefaultGroupPlugins, H> {
    /// Construct a `User` on [`DefaultProvider`] with the default config.
    pub fn with_private_key(
        private_key: &str,
        consensus_service: Arc<ProviderConsensus<DefaultProvider>>,
        handler: Arc<H>,
    ) -> Result<Self, UserError> {
        Self::with_private_key_and_config(
            private_key,
            consensus_service,
            handler,
            GroupConfig::default(),
        )
    }

    /// Construct a `User` on [`DefaultProvider`] with a custom config.
    pub fn with_private_key_and_config(
        private_key: &str,
        consensus_service: Arc<ProviderConsensus<DefaultProvider>>,
        handler: Arc<H>,
        default_group_config: GroupConfig,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = signer.address();

        let identity: Arc<dyn Identity> = Arc::new(WalletIdentity::from_wallet(user_address));
        let credentials = Arc::new(MlsCredentials::from_identity(identity.as_ref())?);
        let plugins = Arc::new(DefaultGroupPlugins {
            storage: Arc::new(MemoryDeMlsStorage::new()),
            credentials,
        });

        Ok(Self::new_with_config(
            identity,
            plugins,
            consensus_service,
            signer,
            handler,
            default_group_config,
        ))
    }
}
