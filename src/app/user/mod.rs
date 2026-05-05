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
        IdentityProvider, KeyPackageBytes, MemoryDeMlsStorage, MlsError, MlsService,
        OpenMlsService, WalletIdentity,
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

pub(crate) struct GroupEntry<M: MlsService> {
    group: Group,
    state_machine: GroupStateMachine,
    /// Per-group MLS service. `None` for joiners in `PendingJoin` state
    /// (no welcome accepted yet); `Some` after the joiner attaches a
    /// service via [`User::mls_welcome_factory`] or for groups created
    /// locally via [`User::mls_creator_factory`].
    mls: Option<Arc<M>>,
    /// Per-group rolling history of committed batches, most recent last.
    /// Bounded at [`MAX_EPOCH_HISTORY`]. Populated by
    /// [`GroupEntry::archive_committed_batch`] from the snapshot
    /// returned by `Group::clear_approved_proposals`.
    epoch_history: VecDeque<HashMap<ProposalId, GroupUpdateRequest>>,
}

impl<M: MlsService> GroupEntry<M> {
    /// Build a fresh entry. Internal-only auxiliary state (epoch
    /// history, future per-entry caches) starts empty.
    pub(crate) fn new(group: Group, state_machine: GroupStateMachine, mls: Option<Arc<M>>) -> Self {
        Self {
            group,
            state_machine,
            mls,
            epoch_history: VecDeque::new(),
        }
    }

    /// Borrow the MLS service for this group, if attached.
    pub(crate) fn mls(&self) -> Option<&Arc<M>> {
        self.mls.as_ref()
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

/// Per-user registry of group entries, with one outer lock for map CRUD
/// and one inner lock per entry so writes on one group don't block reads
/// on another.
type GroupRegistry<M> = Arc<RwLock<HashMap<String, Arc<RwLock<GroupEntry<M>>>>>>;

/// Factory closure that builds an [`MlsService`] for a fresh group.
pub type MlsCreatorFactory<M> = Arc<dyn Fn(String) -> Result<M, MlsError> + Send + Sync + 'static>;
/// Factory closure that tries to build an [`MlsService`] from a serialized
/// MLS welcome. Returns `Ok(None)` when the welcome isn't for us.
pub type MlsWelcomeFactory<M> =
    Arc<dyn Fn(&[u8]) -> Result<Option<M>, MlsError> + Send + Sync + 'static>;
/// Factory closure that generates a fresh single-use key package using the
/// user's storage + identity. Independent of any group's MLS service so a
/// joiner can publish a key package before joining.
pub type KeyPackageGenerator =
    Arc<dyn Fn() -> Result<KeyPackageBytes, MlsError> + Send + Sync + 'static>;

pub struct User<P: DeMlsProvider, M: MlsService, H: GroupEventHandler, SCH: StateChangeHandler>
where
    M::Identity: Clone,
{
    /// Local identity, shared with every per-group MLS service this user
    /// runs. Source of truth for "who am I" at the User level — call sites
    /// must read this rather than reaching into a per-group service.
    identity: M::Identity,
    /// Build an MLS service for a brand-new group as its sole creator.
    mls_creator_factory: MlsCreatorFactory<M>,
    /// Try to build an MLS service from a serialized welcome.
    mls_welcome_factory: MlsWelcomeFactory<M>,
    /// Generate a single-use key package using the user's storage + identity.
    kp_generator: KeyPackageGenerator,
    /// Outer lock: map CRUD (insert / remove / iterate names).
    /// Inner per-entry lock: per-group reads and mutations. A write on
    /// group A doesn't block reads on group B.
    ///
    /// Lock-order convention with [`Self::scoring_service`]: never hold
    /// the scoring `Mutex` guard across `lookup_entry` or any acquisition
    /// of the inner entry `RwLock`. Acquire `scoring()` in a single
    /// statement and let the guard drop at the semicolon.
    groups: GroupRegistry<M>,
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
where
    M::Identity: Clone,
{
    fn clone(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            mls_creator_factory: Arc::clone(&self.mls_creator_factory),
            mls_welcome_factory: Arc::clone(&self.mls_welcome_factory),
            kp_generator: Arc::clone(&self.kp_generator),
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
where
    M::Identity: Clone,
{
    #[allow(clippy::too_many_arguments)]
    fn new_with_config(
        identity: M::Identity,
        mls_creator_factory: MlsCreatorFactory<M>,
        mls_welcome_factory: MlsWelcomeFactory<M>,
        kp_generator: KeyPackageGenerator,
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
            identity,
            mls_creator_factory,
            mls_welcome_factory,
            kp_generator,
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
    pub(crate) async fn lookup_entry(
        &self,
        group_name: &str,
    ) -> Option<Arc<RwLock<GroupEntry<M>>>> {
        self.groups.read().await.get(group_name).cloned()
    }

    /// Run `f` under the entry's own read lock. Returns `None` if the
    /// entry isn't present.
    pub(crate) async fn with_entry<R>(
        &self,
        group_name: &str,
        f: impl FnOnce(&GroupEntry<M>) -> R,
    ) -> Option<R> {
        let entry_arc = self.lookup_entry(group_name).await?;
        let entry = entry_arc.read().await;
        Some(f(&entry))
    }

    /// Borrow the local identity. Source of truth for "who am I" at the
    /// User level.
    pub(crate) fn identity(&self) -> &M::Identity {
        &self.identity
    }

    /// Wallet address as checksummed hex.
    pub fn identity_string(&self) -> String {
        self.identity.identity_display().to_string()
    }

    /// Generate a single-use key package for our identity.
    pub(crate) fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        (self.kp_generator)()
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

/// MLS service type for the default `DefaultProvider`-backed `User`. Uses
/// in-memory storage and a wallet-keyed identity, both shared across every
/// per-group service via `Arc` (the `Arc<S>: DeMlsStorage` and
/// `Arc<I>: IdentityProvider` blanket impls make this work).
pub type DefaultMlsService = OpenMlsService<Arc<MemoryDeMlsStorage>, Arc<WalletIdentity>>;

impl<H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<DefaultProvider, DefaultMlsService, H, SCH>
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

        let identity = Arc::new(WalletIdentity::from_wallet(user_address)?);
        let storage = Arc::new(MemoryDeMlsStorage::new());

        let mls_creator_factory: MlsCreatorFactory<DefaultMlsService> = {
            let storage = Arc::clone(&storage);
            let identity = Arc::clone(&identity);
            Arc::new(move |group_id: String| {
                OpenMlsService::new_as_creator(
                    group_id,
                    Arc::clone(&storage),
                    Arc::clone(&identity),
                )
            })
        };
        let mls_welcome_factory: MlsWelcomeFactory<DefaultMlsService> = {
            let storage = Arc::clone(&storage);
            let identity = Arc::clone(&identity);
            Arc::new(move |bytes: &[u8]| {
                OpenMlsService::new_from_welcome(bytes, Arc::clone(&storage), Arc::clone(&identity))
            })
        };
        let kp_generator: KeyPackageGenerator = {
            let storage = Arc::clone(&storage);
            let identity = Arc::clone(&identity);
            Arc::new(move || {
                OpenMlsService::<Arc<MemoryDeMlsStorage>, Arc<WalletIdentity>>::generate_key_package(
                    &storage, &identity,
                )
            })
        };

        Ok(Self::new_with_config(
            identity,
            mls_creator_factory,
            mls_welcome_factory,
            kp_generator,
            consensus_service,
            signer,
            handler,
            state_handler,
            default_group_config,
        ))
    }
}
