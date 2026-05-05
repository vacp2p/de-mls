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

use std::{
    collections::{HashMap, VecDeque},
    str::FromStr,
    sync::{Arc, Mutex},
};

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::storage::ConsensusStorage;
use tokio::{sync::RwLock, task::JoinHandle};

use crate::{
    app::{
        FixedScoringProvider, GroupConfig, GroupStateMachine, InMemoryPeerScoreStorage,
        PeerScoringService, StateChangeHandler, UserError,
    },
    core::{
        DeMlsProvider, DefaultProvider, Group, GroupEventHandler, ProposalId, ProviderConsensus,
        ScoringConfig,
    },
    mls_crypto::{
        IdentityProvider, MemoryDeMlsStorage, MlsService, OpenMlsService, WalletIdentity,
    },
    protos::de_mls::messages::v1::GroupUpdateRequest,
};

mod consensus;
mod consensus_events;
mod freeze;
mod inbound;
mod lifecycle;
mod messaging;
mod query;
mod steward;

/// Cap on the per-group UI history of committed batches. App-layer
/// concern only; protocol logic in `core::Group` is history-agnostic.
const MAX_EPOCH_HISTORY: usize = 10;

pub(crate) struct GroupEntry {
    group: Group,
    state_machine: GroupStateMachine,
    /// Per-group rolling history of committed batches, most recent last.
    /// Bounded at [`MAX_EPOCH_HISTORY`]. Populated by
    /// [`GroupEntry::archive_committed_batch`] from the snapshot
    /// returned by `Group::clear_approved_proposals`.
    epoch_history: VecDeque<HashMap<ProposalId, GroupUpdateRequest>>,
}

impl GroupEntry {
    /// Build a fresh entry. Internal-only auxiliary state (epoch
    /// history, future per-entry caches) starts empty.
    pub(crate) fn new(group: Group, state_machine: GroupStateMachine) -> Self {
        Self {
            group,
            state_machine,
            epoch_history: VecDeque::new(),
        }
    }

    /// Append a just-committed batch to the bounded UI history.
    fn archive_committed_batch(&mut self, snapshot: HashMap<ProposalId, GroupUpdateRequest>) {
        if snapshot.is_empty() {
            return;
        }
        if self.epoch_history.len() >= MAX_EPOCH_HISTORY {
            self.epoch_history.pop_front();
        }
        self.epoch_history.push_back(snapshot);
    }
}

/// Registry of outstanding auto-vote timers, keyed by
/// `(group_name, proposal_id)`. Cancelled on manual vote, consensus
/// resolution, or group leave. Outer key is the group name (`Arc<str>` so
/// `&str` lookups don't allocate); inner key is the proposal id.
type AutoVoteTimers = Arc<Mutex<HashMap<Arc<str>, HashMap<u32, JoinHandle<()>>>>>;

pub struct User<P: DeMlsProvider, M: MlsService, H: GroupEventHandler, SCH: StateChangeHandler> {
    mls_service: Arc<M>,
    /// Outer lock: map CRUD (insert / remove / iterate names).
    /// Inner per-entry lock: per-group reads and mutations. A write on
    /// group A doesn't block reads on group B.
    ///
    /// Lock-order convention with [`Self::scoring_service`]: never hold
    /// the scoring `Mutex` guard across `lookup_entry` or any acquisition
    /// of the inner entry `RwLock`. Acquire `scoring()` in a single
    /// statement and let the guard drop at the semicolon.
    groups: Arc<RwLock<HashMap<String, Arc<RwLock<GroupEntry>>>>>,
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

impl<P: DeMlsProvider, M: MlsService, H: GroupEventHandler, SCH: StateChangeHandler> Clone
    for User<P, M, H, SCH>
{
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

impl<
    P: DeMlsProvider,
    M: MlsService,
    H: GroupEventHandler + 'static,
    SCH: StateChangeHandler + 'static,
> User<P, M, H, SCH>
{
    fn new_with_config(
        mls_service: M,
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

    /// Look up a group entry. Returns `None` when the entry isn't present.
    /// Takes the outer read lock briefly to clone the inner `Arc`, then
    /// releases it before the caller acquires the entry's own lock.
    pub(crate) async fn lookup_entry(&self, group_name: &str) -> Option<Arc<RwLock<GroupEntry>>> {
        self.groups.read().await.get(group_name).cloned()
    }

    /// Run `f` under the entry's own read lock. Returns `None` if the
    /// entry isn't present.
    pub(crate) async fn with_entry<R>(
        &self,
        group_name: &str,
        f: impl FnOnce(&GroupEntry) -> R,
    ) -> Option<R> {
        let entry_arc = self.lookup_entry(group_name).await?;
        let entry = entry_arc.read().await;
        Some(f(&entry))
    }

    /// Wallet address as checksummed hex.
    pub fn identity_string(&self) -> String {
        self.mls_service.identity().identity_display().to_string()
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
    User<DefaultProvider, OpenMlsService<MemoryDeMlsStorage, WalletIdentity>, H, SCH>
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

        let identity = WalletIdentity::from_wallet(user_address)?;
        let mls_service = OpenMlsService::new(MemoryDeMlsStorage::new(), identity);

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
