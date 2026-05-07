//! [`User`] — multi-group facade over core. One node owns one `User`, which
//! holds the consensus service, event handler, and per-group state map.
//! Each group's MLS service and peer-scoring service live on the
//! [`GroupEntry`] (one instance per group). Methods split across the
//! submodules: `lifecycle` (create/leave), `query` (read-only getters),
//! `messaging` (send/ban), `consensus` (voting), `consensus_events`
//! (outcome dispatch), `inbound` (packet dispatch), `freeze` (timers),
//! `steward` (steward-side housekeeping).
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
        StateChangeHandler, UserError,
    },
    core::{
        DeMlsProvider, DefaultProvider, DeterministicStewardList, Group, GroupEventHandler,
        PeerScoringEvent, PeerScoringPlugin, PeerScoringService, ProposalId, ProviderConsensus,
        ScoringConfig, StewardListConfig, StewardListPlugin,
    },
    identity::{Identity, WalletIdentity},
    mls_crypto::{
        KeyPackageBytes, MemoryDeMlsStorage, MlsCredentials, MlsError, MlsService, OpenMlsService,
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

pub(crate) struct GroupEntry<M: MlsService, Sc: PeerScoringPlugin, St: StewardListPlugin> {
    /// `Group<M>` owns the per-group MLS service inside its own
    /// `Option<M>` slot — joiners in `PendingJoin` carry `None` until a
    /// welcome arrives, at which point the User attaches the service via
    /// [`Group::attach_mls`].
    group: Group<M>,
    state_machine: GroupStateMachine,
    /// Per-group peer-score plug-in. Lives next to `state_machine` as
    /// app-layer wiring; protocol decisions read it via the entry's
    /// `RwLock`, no separate `Mutex` needed.
    scoring: Sc,
    /// Per-group steward list plug-in. Holds the active list, retry
    /// counter, and election retry policy. Coordinator composes
    /// eligibility from MLS members + `Group::is_pending_removal` and
    /// passes it on every position query.
    steward: St,
    /// Per-group rolling history of committed batches, most recent last.
    /// Bounded at [`MAX_EPOCH_HISTORY`]. Populated by
    /// [`GroupEntry::archive_committed_batch`] from the snapshot
    /// returned by `Group::clear_approved_proposals`.
    epoch_history: VecDeque<HashMap<ProposalId, GroupUpdateRequest>>,
}

impl<M: MlsService, Sc: PeerScoringPlugin, St: StewardListPlugin> GroupEntry<M, Sc, St> {
    /// Build a fresh entry. Internal-only auxiliary state (epoch
    /// history, future per-entry caches) starts empty.
    pub(crate) fn new(
        group: Group<M>,
        state_machine: GroupStateMachine,
        scoring: Sc,
        steward: St,
    ) -> Self {
        Self {
            group,
            state_machine,
            scoring,
            steward,
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

/// `true` iff `events` contains at least one downward threshold cross —
/// the signal coordinators react to by chaining into score-removal
/// initiation. Helper kept here so every callsite uses the same
/// triggering rule.
pub(crate) fn has_downward_cross(events: &[PeerScoringEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, PeerScoringEvent::ThresholdCrossedDown { .. }))
}

/// Registry of outstanding auto-vote timers, keyed by
/// `(group_name, proposal_id)`. Cancelled on manual vote, consensus
/// resolution, or group leave. Outer key is the group name (`Arc<str>` so
/// `&str` lookups don't allocate); inner key is the proposal id.
type AutoVoteTimers = Arc<Mutex<HashMap<Arc<str>, HashMap<u32, JoinHandle<()>>>>>;

/// Per-user registry of group entries, with one outer lock for map CRUD
/// and one inner lock per entry so writes on one group don't block reads
/// on another.
type GroupRegistry<M, Sc, St> = Arc<RwLock<HashMap<String, Arc<RwLock<GroupEntry<M, Sc, St>>>>>>;

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
/// Factory closure that builds a [`PeerScoringPlugin`] instance for a
/// fresh group. Currently invoked with the user's default
/// [`ScoringConfig`] derived from `default_group_config`; the parameter
/// is forward-looking for per-group config overrides.
pub type ScoringFactory<Sc> = Arc<dyn Fn(&ScoringConfig) -> Sc + Send + Sync + 'static>;
/// Factory closure that builds a [`StewardListPlugin`] instance for a
/// fresh group. Receives `(group_id_bytes, protocol_config)` and
/// returns an empty plug-in. Lifecycle then bootstraps via
/// [`StewardListPlugin::install_list`] for the creator path or leaves
/// it empty for joiners (filled when `GroupSync` arrives).
pub type StewardFactory<St> = Arc<dyn Fn(&[u8], StewardListConfig) -> St + Send + Sync + 'static>;

pub struct User<
    P: DeMlsProvider,
    M: MlsService,
    Sc: PeerScoringPlugin,
    St: StewardListPlugin,
    I: Identity,
    H: GroupEventHandler,
    SCH: StateChangeHandler,
> {
    /// Local user-level identity, shared across all this user's groups.
    /// Source of truth for "who am I"; MLS state lives in the per-group
    /// service, MLS credentials are captured by the factories below
    /// (built once from this identity at User init).
    identity: Arc<I>,
    /// Build an MLS service for a brand-new group as its sole creator.
    mls_creator_factory: MlsCreatorFactory<M>,
    /// Try to build an MLS service from a serialized welcome.
    mls_welcome_factory: MlsWelcomeFactory<M>,
    /// Generate a single-use key package using the user's storage + credentials.
    kp_generator: KeyPackageGenerator,
    /// Build a fresh peer-scoring plug-in for a new group entry.
    scoring_factory: ScoringFactory<Sc>,
    /// Build a fresh steward-list plug-in for a new group entry.
    steward_factory: StewardFactory<St>,
    /// Outer lock: map CRUD (insert / remove / iterate names).
    /// Inner per-entry lock: per-group reads and mutations. A write on
    /// group A doesn't block reads on group B. Per-group peer scoring
    /// lives inside the entry, so scoring access is guarded by the same
    /// `RwLock` — no separate scoring lock.
    groups: GroupRegistry<M, Sc, St>,
    consensus_service: Arc<ProviderConsensus<P>>,
    eth_signer: PrivateKeySigner,
    handler: Arc<H>,
    state_handler: Arc<SCH>,
    default_group_config: GroupConfig,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped.
    app_id: Vec<u8>,
    /// Per-proposal auto-vote timers. Spawned when a proposal first becomes
    /// visible locally (own submit or peer inbound); cancelled on manual
    /// vote, consensus resolution, or group leave.
    auto_vote_timers: AutoVoteTimers,
}

impl<
    P: DeMlsProvider,
    M: MlsService,
    Sc: PeerScoringPlugin,
    St: StewardListPlugin,
    I: Identity,
    H: GroupEventHandler,
    SCH: StateChangeHandler,
> Clone for User<P, M, Sc, St, I, H, SCH>
{
    fn clone(&self) -> Self {
        Self {
            identity: Arc::clone(&self.identity),
            mls_creator_factory: Arc::clone(&self.mls_creator_factory),
            mls_welcome_factory: Arc::clone(&self.mls_welcome_factory),
            kp_generator: Arc::clone(&self.kp_generator),
            scoring_factory: Arc::clone(&self.scoring_factory),
            steward_factory: Arc::clone(&self.steward_factory),
            groups: Arc::clone(&self.groups),
            consensus_service: Arc::clone(&self.consensus_service),
            eth_signer: self.eth_signer.clone(),
            handler: Arc::clone(&self.handler),
            state_handler: Arc::clone(&self.state_handler),
            default_group_config: self.default_group_config.clone(),
            app_id: self.app_id.clone(),
            auto_vote_timers: Arc::clone(&self.auto_vote_timers),
        }
    }
}

impl<
    P: DeMlsProvider,
    M: MlsService,
    Sc: PeerScoringPlugin,
    St: StewardListPlugin,
    I: Identity,
    H: GroupEventHandler + 'static,
    SCH: StateChangeHandler + 'static,
> User<P, M, Sc, St, I, H, SCH>
{
    #[allow(clippy::too_many_arguments)]
    fn new_with_config(
        identity: Arc<I>,
        mls_creator_factory: MlsCreatorFactory<M>,
        mls_welcome_factory: MlsWelcomeFactory<M>,
        kp_generator: KeyPackageGenerator,
        scoring_factory: ScoringFactory<Sc>,
        steward_factory: StewardFactory<St>,
        consensus_service: Arc<ProviderConsensus<P>>,
        eth_signer: PrivateKeySigner,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
        default_group_config: GroupConfig,
    ) -> Self {
        Self {
            identity,
            mls_creator_factory,
            mls_welcome_factory,
            kp_generator,
            scoring_factory,
            steward_factory,
            groups: Arc::new(RwLock::new(HashMap::new())),
            consensus_service,
            eth_signer,
            handler,
            state_handler,
            default_group_config,
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            auto_vote_timers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build a fresh per-group scoring plug-in from the user's default
    /// scoring config. Used by entry construction in lifecycle / welcome
    /// paths.
    pub(crate) fn make_scoring_service(&self) -> Sc {
        let scoring_config = ScoringConfig {
            default_score: self.default_group_config.default_peer_score,
            threshold: self.default_group_config.threshold_peer_score,
        };
        (self.scoring_factory)(&scoring_config)
    }

    /// Build a fresh per-group steward-list plug-in. Returns an empty
    /// plug-in; lifecycle creator path bootstraps via `install_list`,
    /// joiner path leaves it empty until `GroupSync` arrives.
    pub(crate) fn make_steward_plugin(&self, group_name: &str, config: &StewardListConfig) -> St {
        (self.steward_factory)(group_name.as_bytes(), config.clone())
    }

    /// Look up a group entry. Returns `None` when the entry isn't present.
    /// Takes the outer read lock briefly to clone the inner `Arc`, then
    /// releases it before the caller acquires the entry's own lock.
    pub(crate) async fn lookup_entry(
        &self,
        group_name: &str,
    ) -> Option<Arc<RwLock<GroupEntry<M, Sc, St>>>> {
        self.groups.read().await.get(group_name).cloned()
    }

    /// Run `f` under the entry's own read lock. Returns `None` if the
    /// entry isn't present.
    pub(crate) async fn with_entry<R>(
        &self,
        group_name: &str,
        f: impl FnOnce(&GroupEntry<M, Sc, St>) -> R,
    ) -> Option<R> {
        let entry_arc = self.lookup_entry(group_name).await?;
        let entry = entry_arc.read().await;
        Some(f(&entry))
    }

    /// Borrow the local identity. Source of truth for "who am I" at the
    /// User level.
    pub(crate) fn identity(&self) -> &I {
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
/// in-memory storage shared across every per-group service via `Arc` (the
/// `Arc<S>: DeMlsStorage` blanket impl makes this work). MLS credentials
/// live separately on `User` and are passed in at service construction.
pub type DefaultMlsService = OpenMlsService<Arc<MemoryDeMlsStorage>>;

/// Peer-scoring plug-in type for the default-config `User`: the
/// reference [`PeerScoringService`] over in-memory storage and a
/// fixed-table provider.
pub type DefaultPeerScoring = PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider>;

/// Steward-list plug-in type for the default-config `User`: the
/// reference [`DeterministicStewardList`].
pub type DefaultStewardList = DeterministicStewardList;

impl<H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<
        DefaultProvider,
        DefaultMlsService,
        DefaultPeerScoring,
        DefaultStewardList,
        WalletIdentity,
        H,
        SCH,
    >
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

        let identity = Arc::new(WalletIdentity::from_wallet(user_address));
        let mls_credentials = Arc::new(MlsCredentials::from_identity(identity.as_ref())?);
        let storage = Arc::new(MemoryDeMlsStorage::new());

        let mls_creator_factory: MlsCreatorFactory<DefaultMlsService> = {
            let storage = Arc::clone(&storage);
            let credentials = Arc::clone(&mls_credentials);
            Arc::new(move |group_id: String| {
                OpenMlsService::new_as_creator(
                    group_id,
                    Arc::clone(&storage),
                    Arc::clone(&credentials),
                )
            })
        };
        let mls_welcome_factory: MlsWelcomeFactory<DefaultMlsService> = {
            let storage = Arc::clone(&storage);
            let credentials = Arc::clone(&mls_credentials);
            Arc::new(move |bytes: &[u8]| {
                OpenMlsService::new_from_welcome(
                    bytes,
                    Arc::clone(&storage),
                    Arc::clone(&credentials),
                )
            })
        };
        let kp_generator: KeyPackageGenerator = {
            let storage = Arc::clone(&storage);
            let credentials = Arc::clone(&mls_credentials);
            Arc::new(move || {
                OpenMlsService::<Arc<MemoryDeMlsStorage>>::generate_key_package(
                    &storage,
                    &credentials,
                )
            })
        };
        let scoring_factory: ScoringFactory<DefaultPeerScoring> =
            Arc::new(|cfg: &ScoringConfig| {
                PeerScoringService::new(
                    InMemoryPeerScoreStorage::new(),
                    FixedScoringProvider::with_default_deltas(),
                    cfg.clone(),
                )
            });
        let steward_factory: StewardFactory<DefaultStewardList> =
            Arc::new(|group_id: &[u8], config: StewardListConfig| {
                DeterministicStewardList::empty(group_id.to_vec(), config)
            });

        Ok(Self::new_with_config(
            identity,
            mls_creator_factory,
            mls_welcome_factory,
            kp_generator,
            scoring_factory,
            steward_factory,
            consensus_service,
            signer,
            handler,
            state_handler,
            default_group_config,
        ))
    }
}
