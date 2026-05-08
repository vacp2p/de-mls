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
    collections::HashMap,
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::storage::ConsensusStorage;
use prost::Message;
use tokio::{sync::RwLock, task::JoinHandle};
use tracing::info;

use crate::{
    app::{FixedScoringProvider, GroupConfig, InMemoryPeerScoreStorage, PhaseTimer, UserError},
    core::{
        BufferedCommitCandidate, CoreError, DeMlsProvider, DefaultProvider,
        DeterministicStewardList, FreezeFinalizeResult, Group, GroupEventHandler, GroupState,
        GroupStateMachine, OperatingMode, PeerScoringEvent, PeerScoringPlugin, PeerScoringService,
        ProcessResult, ProposalKind, ProviderConsensus, ScoringConfig, StewardListConfig,
        StewardListPlugin, compute_commit_hash, finalize_freeze_round, member_set, process_inbound,
    },
    ds::{APP_MSG_SUBTOPIC, OutboundPacket},
    identity::{Identity, WalletIdentity},
    mls_crypto::{
        CommitCandidate as MlsCommitCandidate, KeyPackageBytes, MemoryDeMlsStorage, MlsCommitInput,
        MlsCredentials, MlsError, MlsService, OpenMlsService,
    },
    protos::de_mls::messages::v1::{AppMessage, CommitCandidate, group_update_request::Payload},
};

mod consensus;
mod consensus_events;
mod freeze;
mod inbound;
mod lifecycle;
mod messaging;
mod query;
mod steward;

pub(crate) struct GroupEntry<M: MlsService, Sc: PeerScoringPlugin, St: StewardListPlugin> {
    group: Group,
    /// Per-group MLS service. `None` for joiners in `PendingJoin` who
    /// haven't accepted a welcome yet; once attached via
    /// [`Self::attach_mls`] it stays `Some` for the entry's lifetime.
    mls: Option<M>,
    /// Per-group state machine. Coordinator methods on this entry update
    /// it together with `phase_timer` so the two never drift.
    state_machine: GroupStateMachine,
    /// Wall-clock anchor + phase-anchor durations combined with
    /// `state_machine` by the entry's coordinator methods.
    phase_timer: PhaseTimer,
    /// Per-group durable config: voting/consensus durations,
    /// `liveness_criteria_yes`, `pending_update_max_epochs`. Read by
    /// app-layer coordinators; joiner-sync writes through this directly.
    pub(crate) config: GroupConfig,
    /// Per-group peer-score plug-in. Lives next to `state_machine` as
    /// app-layer wiring; protocol decisions read it via the entry's
    /// `RwLock`, no separate `Mutex` needed.
    scoring: Sc,
    /// Per-group steward list plug-in. Holds the active list, retry
    /// counter, and election retry policy. Coordinator composes
    /// eligibility from MLS members + `Group::is_pending_removal` and
    /// passes it on every position query.
    steward: St,
    /// Authorization mode (RFC §Layer 3 Anti-Deadlock ECP). `Recovery` is
    /// set when an accepted Deadlock ECP relaxes the steward gate so any
    /// member may produce the next commit; cleared on accepted election.
    /// Read by the freeze coordinator, the create-commit path, and
    /// `core::finalize_freeze_round` (via `in_recovery` parameter).
    operating_mode: OperatingMode,
}

impl<M: MlsService, Sc: PeerScoringPlugin, St: StewardListPlugin> GroupEntry<M, Sc, St> {
    /// Build a fresh entry. Creator path passes `Some(mls)`; joiner
    /// path passes `None` and attaches later via [`Self::attach_mls`].
    pub(crate) fn new(
        group: Group,
        mls: Option<M>,
        state_machine: GroupStateMachine,
        phase_timer: PhaseTimer,
        config: GroupConfig,
        scoring: Sc,
        steward: St,
    ) -> Self {
        Self {
            group,
            mls,
            state_machine,
            phase_timer,
            config,
            scoring,
            steward,
            operating_mode: OperatingMode::Normal,
        }
    }

    // ── Operating mode (Layer 3 Anti-Deadlock) ──────────────────────

    pub(crate) fn is_in_recovery_mode(&self) -> bool {
        self.operating_mode == OperatingMode::Recovery
    }

    pub(crate) fn enter_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Recovery;
    }

    pub(crate) fn exit_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Normal;
    }

    // ── State-machine + phase-timer coordinators ────────────────────

    pub(crate) fn current_state(&self) -> GroupState {
        self.state_machine.current_state()
    }

    pub(crate) fn start_working(&mut self) -> GroupState {
        self.state_machine.start_working();
        self.phase_timer.clear();
        info!(state = "Working", "state transition");
        GroupState::Working
    }

    pub(crate) fn start_freezing(&mut self) -> GroupState {
        self.state_machine.start_freezing();
        self.phase_timer.start();
        info!(state = "Freezing", "state transition");
        GroupState::Freezing
    }

    /// Bypass the inactivity timer and enter Freezing immediately. Returns
    /// `Some(Freezing)` on transition (only fires from Working or
    /// Reelection); `None` from other states.
    pub(crate) fn force_freezing(&mut self) -> Option<GroupState> {
        if self.state_machine.force_freezing() {
            self.phase_timer.start();
            info!(state = "Freezing", "state transition (forced)");
            Some(GroupState::Freezing)
        } else {
            None
        }
    }

    pub(crate) fn start_selection(&mut self) -> GroupState {
        self.state_machine.start_selection();
        info!(state = "Selection", "state transition");
        GroupState::Selection
    }

    pub(crate) fn start_reelection(&mut self) -> GroupState {
        self.state_machine.start_reelection();
        self.phase_timer.clear();
        info!(state = "Reelection", "state transition");
        GroupState::Reelection
    }

    /// `true` once 3× `commit_inactivity_duration` has passed in
    /// `PendingJoin` without a welcome.
    ///
    /// Pipeline: consensus (~15s) + commit_inactivity + freeze (≈ commit/2)
    /// = ~1.5× commit-inactivity + consensus overhead. Use 3× for safety margin.
    pub(crate) fn is_pending_join_expired(&self) -> bool {
        self.state_machine.current_state() == GroupState::PendingJoin
            && self
                .phase_timer
                .elapsed_since_anchor(self.config.commit_inactivity_duration * 3)
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub(crate) fn is_freeze_timed_out(&self) -> bool {
        self.state_machine.current_state() == GroupState::Freezing
            && self
                .phase_timer
                .elapsed_since_anchor(self.config.freeze_duration)
    }

    /// Drives the "steward waited too long to commit" transition into
    /// `Freezing`. Call each poll tick. Returns `Some(Freezing)` exactly
    /// on the tick that transitions; `None` while still waiting, outside
    /// Working, or when there's no approved work. Self-starts the
    /// inactivity anchor on the first tick with approved work.
    pub(crate) fn check_steward_inactivity(
        &mut self,
        approved_proposals_count: usize,
        inactivity_duration: Duration,
    ) -> Option<GroupState> {
        if self.state_machine.current_state() != GroupState::Working
            || approved_proposals_count == 0
        {
            return None;
        }
        if self.phase_timer.started_at().is_none() {
            self.phase_timer.start();
            info!(
                approved = approved_proposals_count,
                inactivity_ms = inactivity_duration.as_millis() as u64,
                "inactivity timer started"
            );
            return None;
        }
        if !self.phase_timer.elapsed_since_anchor(inactivity_duration) {
            return None;
        }
        info!(
            inactivity_ms = inactivity_duration.as_millis() as u64,
            approved = approved_proposals_count,
            "inactivity window elapsed, entering freeze"
        );
        Some(self.start_freezing())
    }

    /// Borrow the MLS service, if attached. `None` for joiners
    /// pre-welcome.
    pub(crate) fn mls(&self) -> Option<&M> {
        self.mls.as_ref()
    }

    /// Borrow the MLS service, erroring with
    /// [`crate::core::CoreError::MlsGroupNotInitialized`] when not
    /// attached. Use this in code paths where the service must be
    /// present so the `?` chain stays linear.
    pub(crate) fn expect_mls(&self) -> Result<&M, crate::core::CoreError> {
        self.mls
            .as_ref()
            .ok_or(crate::core::CoreError::MlsGroupNotInitialized)
    }

    /// Attach an MLS service. Called by joiners after the welcome
    /// arrives. Caller is responsible for not double-attaching.
    pub(crate) fn attach_mls(&mut self, mls: M) {
        self.mls = Some(mls);
    }

    /// Drop the attached MLS service and return it. Used on group leave
    /// so the caller can run service-side cleanup (`mls.delete()`).
    pub(crate) fn take_mls(&mut self) -> Option<M> {
        self.mls.take()
    }

    // ── Protocol-function wrappers ─────────────────────────────────
    //
    // Pull `group`, `mls`, and `steward` from `self` so coordinator
    // callsites don't destructure the entry. Protocol logic stays in
    // `core::api`; these are pure delegation.

    /// Current MLS members; empty when no service is attached
    /// (joiner pre-welcome).
    pub(crate) fn group_members(&self) -> Result<Vec<Vec<u8>>, CoreError> {
        match &self.mls {
            Some(mls) => Ok(mls.members()?),
            None => Err(CoreError::MlsGroupNotInitialized),
        }
    }

    /// Build a commit candidate. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached.
    pub(crate) fn create_commit_candidate(
        &mut self,
        self_identity: &[u8],
        app_id: &[u8],
    ) -> Result<Option<OutboundPacket>, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        if !self.steward.is_steward(self_identity) && !self.is_in_recovery_mode() {
            return Err(CoreError::NotASteward);
        }

        if self.group.approved_proposals().is_empty() {
            return Err(CoreError::NoProposals);
        }

        // MLS forbids committing one's own removal. If the approved batch contains
        // RemoveMember(self), skip local candidate creation — another steward will
        // commit the batch (including this node's removal) once they enter freeze.
        let self_removal_pending = self.group.approved_proposals().values().any(|req| {
            matches!(
                req.payload.as_ref(),
                Some(Payload::RemoveMember(r))
                    if r.identity == self_identity
            )
        });
        if self_removal_pending {
            info!(
                group = self.group.group_name(),
                "commit candidate skipped: approved batch contains self-remove"
            );
            return Ok(None);
        }

        // Governance proposals (emergency, election) are consensus-only and must
        // not be in the approved queue at batch creation time.
        let non_mls_ids: Vec<u32> = self
            .group
            .approved_proposals()
            .iter()
            .filter(|(_, req)| ProposalKind::of(req).is_governance())
            .map(|(&id, _)| id)
            .collect();

        if !non_mls_ids.is_empty() {
            return Err(CoreError::UnexpectedNonMlsProposals {
                proposal_ids: non_mls_ids,
            });
        }

        // Drop approved entries already reflected in group state (stale
        // rebroadcast KPs, duplicate removes) — without this MLS would reject
        // the whole batch with "Duplicate signature key in proposals and group".
        let current_members = mls.members()?;
        let current_members_set = member_set(&current_members);
        let is_member = |id: &[u8]| current_members_set.contains(id);

        // Urgent (ECP-driven) freeze: restrict the batch to just the target's
        // RemoveMember. See `Group::urgent_commit_target`.
        let urgent_target = self.group.urgent_commit_target().map(|t| t.to_vec());

        // Iterate in insertion order (FIFO): library proposal IDs are
        // content-derived hashes, so sort-by-id is not temporal.
        let k_max = mls.commit_batch_max();
        let mut updates = Vec::with_capacity(self.group.approved_order().len().min(k_max));
        for pid in self.group.approved_order() {
            if updates.len() >= k_max {
                break;
            }
            let Some(proposal) = self.group.approved_proposals().get(pid) else {
                continue;
            };
            match proposal.payload.as_ref() {
                Some(Payload::InviteMember(im)) => {
                    if urgent_target.is_some() {
                        continue;
                    }
                    if is_member(&im.identity) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Add(KeyPackageBytes::new(
                        im.key_package_bytes.clone(),
                        im.identity.clone(),
                    )));
                }
                Some(Payload::RemoveMember(rm)) => {
                    if let Some(target) = urgent_target.as_deref()
                        && rm.identity != target
                    {
                        continue;
                    }
                    if !is_member(&rm.identity) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Remove(rm.identity.clone()));
                }
                _ => return Err(CoreError::InvalidGroupUpdateRequest),
            }
        }

        if updates.is_empty() {
            return Ok(None);
        }

        let MlsCommitCandidate {
            proposals: mls_proposals,
            commit,
            welcome,
        } = mls.create_commit_candidate(&updates)?;

        let candidate = CommitCandidate {
            group_name: self.group.group_name_bytes().to_vec(),
            mls_proposals,
            commit_message: commit,
            steward_identity: self_identity.to_vec(),
        };

        // Welcome bytes are deferred: sent from finalize_freeze_round after the
        // commit merges, so joiners can't advance epoch ahead of the steward.
        let commit_hash = compute_commit_hash(&candidate.commit_message);
        let epoch = mls.current_epoch()?;
        let _ = self.group.add_freeze_candidate(
            BufferedCommitCandidate {
                candidate_msg: candidate.clone(),
                commit_hash,
                is_local_candidate: true,
                welcome_bytes: welcome,
            },
            epoch,
        );

        info!(
            group = self.group.group_name(),
            epoch,
            proposals = updates.len(),
            "commit candidate created"
        );

        let candidate_msg: AppMessage = candidate.into();
        Ok(Some(OutboundPacket::new(
            candidate_msg.encode_to_vec(),
            APP_MSG_SUBTOPIC,
            self.group.group_name(),
            app_id,
        )))
    }

    /// Finalize the active freeze round. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached.
    pub(crate) fn finalize_freeze_round(
        &mut self,
        allow_subset_candidates: bool,
        app_id: &[u8],
    ) -> Result<FreezeFinalizeResult, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        let in_recovery = self.operating_mode == OperatingMode::Recovery;
        finalize_freeze_round(
            &mut self.group,
            mls,
            &self.steward,
            in_recovery,
            allow_subset_candidates,
            app_id,
        )
    }

    /// Process an inbound app-subtopic payload. Errors with
    /// [`CoreError::MlsGroupNotInitialized`] when no MLS service is
    /// attached — caller should check `mls().is_some()` first.
    pub(crate) fn process_inbound(&mut self, payload: &[u8]) -> Result<ProcessResult, CoreError> {
        let mls = self.mls.as_ref().ok_or(CoreError::MlsGroupNotInitialized)?;
        process_inbound(&mut self.group, mls, payload)
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
> Clone for User<P, M, Sc, St, I, H>
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
> User<P, M, Sc, St, I, H>
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

impl<H: GroupEventHandler + 'static>
    User<
        DefaultProvider,
        DefaultMlsService,
        DefaultPeerScoring,
        DefaultStewardList,
        WalletIdentity,
        H,
    >
{
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
            default_group_config,
        ))
    }
}
