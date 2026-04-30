//! [`User`] — multi-group facade over core. One node owns one `User`, which
//! holds the MLS service, consensus service, event handler, peer-scoring
//! service, and per-group state map. Methods split across the submodules:
//! `lifecycle` (create/leave), `query` (read-only getters), `messaging`
//! (send/ban), `consensus` (voting), `consensus_events` (outcome dispatch),
//! `inbound` (packet dispatch), `freeze` (timers), `steward` (steward-side
//! housekeeping).
//!
//! `User` is `Clone` — all fields are `Arc` or cheap `Clone` — so background
//! tasks just take their own handle via `self.clone()`.

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::storage::ConsensusStorage;
use std::{
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex},
};
use tokio::{sync::RwLock, task::JoinHandle};

use crate::{
    app::{
        FixedScoringProvider, GroupConfig, GroupStateMachine, InMemoryPeerScoreStorage,
        PeerScoringService, StateChangeHandler, UserError,
    },
    core::{
        DeMlsProvider, DefaultProvider, Group, GroupEventHandler, ProviderConsensus, ScoringConfig,
    },
    mls_crypto::{MemoryDeMlsStorage, MlsService},
};

mod consensus;
mod consensus_events;
pub(crate) mod emergency;
mod freeze;
mod inbound;
mod lifecycle;
mod messaging;
mod query;
mod steward;

pub(crate) struct GroupEntry {
    group: Group,
    state_machine: GroupStateMachine,
}

/// Registry of outstanding auto-vote timers, keyed by
/// `(group_name, proposal_id)`. Cancelled on manual vote, consensus
/// resolution, or group leave.
type AutoVoteTimers = Arc<Mutex<HashMap<(String, u32), JoinHandle<()>>>>;

pub struct User<P: DeMlsProvider, H: GroupEventHandler, SCH: StateChangeHandler> {
    mls_service: Arc<MlsService<P::Storage>>,
    groups: Arc<RwLock<HashMap<String, GroupEntry>>>,
    consensus_service: Arc<ProviderConsensus<P>>,
    eth_signer: PrivateKeySigner,
    handler: Arc<H>,
    state_handler: Arc<SCH>,
    default_group_config: GroupConfig,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped.
    app_id: Vec<u8>,
    scoring_service: Arc<Mutex<PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider>>>,
    /// Per-proposal auto-vote timers. Spawned when a proposal first becomes
    /// visible locally (own submit or peer inbound); cancelled on manual
    /// vote, consensus resolution, or group leave.
    auto_vote_timers: AutoVoteTimers,
}

impl<P: DeMlsProvider, H: GroupEventHandler, SCH: StateChangeHandler> Clone for User<P, H, SCH> {
    fn clone(&self) -> Self {
        Self {
            mls_service: Arc::clone(&self.mls_service),
            groups: Arc::clone(&self.groups),
            consensus_service: Arc::clone(&self.consensus_service),
            eth_signer: self.eth_signer.clone(),
            handler: Arc::clone(&self.handler),
            state_handler: Arc::clone(&self.state_handler),
            default_group_config: self.default_group_config.clone(),
            app_id: self.app_id.clone(),
            scoring_service: Arc::clone(&self.scoring_service),
            auto_vote_timers: Arc::clone(&self.auto_vote_timers),
        }
    }
}

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    fn new_with_config(
        mls_service: MlsService<P::Storage>,
        consensus_service: Arc<ProviderConsensus<P>>,
        eth_signer: PrivateKeySigner,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
        default_group_config: GroupConfig,
    ) -> Self {
        let scoring_config = ScoringConfig {
            default_score: default_group_config.default_peer_score,
        };
        Self {
            mls_service: Arc::new(mls_service),
            groups: Arc::new(RwLock::new(HashMap::new())),
            consensus_service,
            eth_signer,
            handler,
            state_handler,
            default_group_config,
            scoring_service: Arc::new(Mutex::new(PeerScoringService::new(
                InMemoryPeerScoreStorage::new(),
                FixedScoringProvider::with_default_deltas(),
                scoring_config,
            ))),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            auto_vote_timers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Lock the scoring service, recovering from a poisoned mutex.
    fn scoring(
        &self,
    ) -> std::sync::MutexGuard<'_, PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider>>
    {
        self.scoring_service
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Read-locked snapshot of one group's entry. Returns `None` when the
    /// entry isn't present — caller chooses between bailing
    /// (`let Some(x) = ... else { return; }`) or escalating
    /// (`.ok_or(UserError::GroupNotFound)?`).
    ///
    /// `f` runs while the read lock is held, so its return type must own
    /// any data taken out of the entry — typical use is a tuple of `Copy`
    /// state-machine fields.
    pub(crate) async fn with_entry<R>(
        &self,
        group_name: &str,
        f: impl FnOnce(&GroupEntry) -> R,
    ) -> Option<R> {
        let groups = self.groups.read().await;
        groups.get(group_name).map(f)
    }

    /// Wallet address as checksummed hex.
    pub fn identity_string(&self) -> String {
        self.mls_service.wallet_hex()
    }

    /// Drop all proposals / votes / sessions for this group from the
    /// consensus service and abort every auto-vote timer belonging to it.
    /// Called on leave, pending-join timeout, and re-creation.
    async fn cleanup_consensus_scope(&self, group_name: &str) -> Result<(), UserError> {
        self.cancel_group_auto_votes(group_name);
        let scope = P::Scope::from(group_name.to_string());
        self.consensus_service
            .storage()
            .delete_scope(&scope)
            .await?;
        Ok(())
    }
}

// ─────────────────────────── DefaultProvider Convenience ───────────────────────────

impl<H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<DefaultProvider, H, SCH>
{
    /// Construct a `User` on [`DefaultProvider`] with the default config.
    pub fn with_private_key(
        private_key: &str,
        consensus_service: Arc<ProviderConsensus<DefaultProvider>>,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
    ) -> Result<Self, UserError> {
        Self::with_private_key_and_config(
            private_key,
            consensus_service,
            handler,
            state_handler,
            GroupConfig::default(),
        )
    }

    /// Construct a `User` on [`DefaultProvider`] with a custom config.
    pub fn with_private_key_and_config(
        private_key: &str,
        consensus_service: Arc<ProviderConsensus<DefaultProvider>>,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
        default_group_config: GroupConfig,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = signer.address();

        let mls_service = MlsService::new(MemoryDeMlsStorage::new());
        mls_service
            .init(user_address)
            .map_err(|e| UserError::Core(e.into()))?;

        Ok(Self::new_with_config(
            mls_service,
            consensus_service,
            signer,
            handler,
            state_handler,
            default_group_config,
        ))
    }
}
