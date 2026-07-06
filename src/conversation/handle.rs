//! [`Conversation`] struct, constructor, and the state-machine + phase-timer
//! coordinators that hold per-conversation protocol state, the MLS service,
//! plug-ins, and durable config alongside [`crate::PhaseTimer`] under
//! one lock. Per-conversation method bodies (proposal submission, voting,
//! inbound dispatch, etc.) live in sibling modules and extend `Conversation`
//! via additional `impl` blocks.

use std::error::Error as StdError;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use openmls_traits::storage::StorageProvider;
use openmls_traits::{OpenMlsProvider, signatures::Signer};
use prost::Message;
use tracing::info;

use crate::{
    CommitRoundResult, ConsensusEngine, ConsensusPlugin, ConversationConfig, ConversationError,
    ConversationEvent, ConversationQueues, ConversationState, ConversationStateMachine,
    OperatingMode, Outbound, PeerScoreStorage, PeerScoringService, PhaseTimer, ProcessResult,
    ProposalKind, StewardListService,
    consensus::outcome_bus::OutcomeReceiver,
    decode_inbound_payload, finalize_commit_round, member_set,
    mls_crypto::{CommitArtifacts, MlsCommitInput, MlsService},
    protos::de_mls::messages::v1::{
        AppMessage, CommitCandidate, conversation_update_request::Payload,
    },
    replay_early_candidates,
};

/// Outcome of [`Conversation::leave`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveOutcome {
    /// A self-leave consensus round has been opened. The conversation stays
    /// active until the next steward commit merges the removal.
    LeaveInitiated,
}

/// One pending auto-vote: cast `vote` for `proposal_id` once the wall-clock
/// catches up to `fire_at`. Registered by `initiate_proposal` (Deferred
/// path) and `on_incoming_proposal`; cancelled on manual vote or consensus
/// resolution; fired by [`Conversation::tick_deadlines`].
#[derive(Debug, Clone, Copy)]
pub struct AutoVoteEntry {
    pub fire_at: Instant,
    pub vote: bool,
}

/// The per-conversation MLS service, plug-in instances, and consensus wiring,
/// grouped so the handle holds them as one unit. Construction builds the MLS
/// service, moves the plug-in instances in, and subscribes the consensus
/// receiver here.
pub(crate) struct ConversationServices<C: ConsensusPlugin, Sc: PeerScoreStorage> {
    /// Per-conversation MLS service. Present for the conversation's whole
    /// lifetime: the creator seeds it at [`Conversation::create`], the joiner
    /// at [`Conversation::join`].
    pub(crate) mls: MlsService,
    /// Per-conversation peer-score tracker.
    pub(crate) scoring: PeerScoringService<Sc>,
    /// Per-conversation steward roster.
    pub(crate) steward_list: StewardListService,
    /// Per-conversation consensus service. Owns this conversation's scope
    /// in the shared storage and a private event bus. Built from the
    /// consensus service passed at construction.
    pub(crate) consensus: ConsensusEngine<C>,
    /// Drain end of `consensus`'s outcome bus. Walked by `tick_deadlines`,
    /// which dispatches each event through `handle_consensus_outcome`.
    /// Subscribed when the conversation is built in [`Conversation::create`] /
    /// [`Conversation::join`].
    pub(crate) consensus_rx: OutcomeReceiver,
}

/// Time-driven conversation state walked once per polling cycle.
pub(crate) struct Timing {
    /// Wall-clock anchor combined with the state machine by coordinator methods.
    pub(crate) phase_timer: PhaseTimer,
    /// Pending auto-votes by `proposal_id`. Walked by
    /// `tick_deadlines`; each entry whose `fire_at` has passed
    /// gets a `cast_vote` and is removed from the map. Cancelled (removed)
    /// when a manual vote arrives or the consensus session resolves.
    pub(crate) pending_auto_votes: HashMap<u32, AutoVoteEntry>,
    /// Pending consensus-session timeouts: `proposal_id -> fire_at`.
    /// Registered when a proposal opens (own or incoming peer); fired by
    /// `tick_deadlines` which calls `consensus.handle_consensus_timeout`.
    /// Removed when the consensus session resolves naturally via `handle_consensus_outcome`.
    pub(crate) pending_consensus_timeouts: HashMap<u32, Instant>,
    /// Last commit-round progress snapshot emitted as `ConversationEvent::CommitRoundProgress`.
    /// `poll()` compares the current `(received, expected)` against this and
    /// emits a new event only when the count changes, avoiding repeated events
    /// on consecutive polling ticks that observe the same progress. Reset to
    /// `None` when the conversation leaves `Freezing`.
    pub(crate) last_commit_round_progress: Option<(usize, usize)>,
    /// Anchor for the backup-steward proposal takeover: set when an
    /// unproposed buffered membership update first appears with no live
    /// proposal for it. A backup steward proposes the buffered update once
    /// this is older than the recovery window — the epoch steward had its
    /// chance and stayed silent. Cleared when the buffer is drained or nothing
    /// is left to propose.
    pub(crate) buffered_propose_anchor: Option<Instant>,
}

impl Timing {
    /// Fresh time-driven state: a cleared phase timer, no pending votes or
    /// timeouts, and no recorded commit-round progress.
    fn new() -> Self {
        Self {
            phase_timer: PhaseTimer::new(),
            pending_auto_votes: HashMap::new(),
            pending_consensus_timeouts: HashMap::new(),
            last_commit_round_progress: None,
            buffered_propose_anchor: None,
        }
    }
}

pub struct Conversation<C: ConsensusPlugin, Sc: PeerScoreStorage> {
    /// Conversation name. Identifies this conversation in the integrator's
    /// registry and is used to construct scope keys for consensus operations.
    /// Read via [`Conversation::conversation_id`].
    pub(crate) conversation_id: String,
    pub(crate) queues: ConversationQueues,
    /// Per-conversation MLS service, plug-in instances, and consensus wiring.
    pub(crate) services: ConversationServices<C, Sc>,
    pub(crate) state_machine: ConversationStateMachine,
    /// Per-conversation durable config: voting/consensus durations,
    /// `liveness_criteria_yes`, `pending_update_max_epochs`.
    pub(crate) config: ConversationConfig,
    /// Authorization mode (RFC §Layer 3 Anti-Deadlock ECP).
    operating_mode: OperatingMode,
    /// Manual+auto recovery rounds elapsed with no commit in the current
    /// recovery episode. Reset on entry; compared against `recovery_max_rounds`
    /// to decide the stop line. Meaningful only while in recovery.
    pub(crate) recovery_rounds: u32,
    /// Time-driven state walked once per polling cycle.
    pub(crate) timing: Timing,
    /// Identity bytes of the local member, snapshotted from the `member_id`
    /// passed at construction. Read via [`Conversation::member_id_bytes`].
    pub(crate) self_member_id: Arc<[u8]>,
    /// Per-instance app id supplied at construction. Tagged on every
    /// outbound packet and used for self-echo filtering in
    /// [`Conversation::process_inbound`]. Read via [`Conversation::app_id`].
    pub(crate) app_id: Arc<[u8]>,
    /// Pending [`ConversationEvent`]s waiting for a caller to drain. Interior
    /// `Mutex` so producer-side `emit_event` stays `&self`; consumers
    /// drain via [`Self::drain_events`] once per polling cycle.
    pending_events: Mutex<Vec<ConversationEvent>>,
    /// Outbound the conversation produced, waiting for the integrator to
    /// publish. The conversation never sends — it buffers here and the caller
    /// drains via [`Self::drain_outbound`] once per cycle. Interior `Mutex`
    /// so producer-side `broadcast` stays `&self`.
    pending_outbound: Mutex<Vec<Outbound>>,
}

impl<C, Sc> Conversation<C, Sc>
where
    C: ConsensusPlugin,
    Sc: PeerScoreStorage,
{
    /// Build a fresh conversation around an already-assembled `services`
    /// bundle (MLS service seeded, plug-ins configured, consensus receiver
    /// subscribed). The time-driven `timing` state starts from defaults.
    pub(crate) fn new(
        conversation_id: String,
        queues: ConversationQueues,
        services: ConversationServices<C, Sc>,
        state_machine: ConversationStateMachine,
        config: ConversationConfig,
        self_member_id: Arc<[u8]>,
        app_id: Arc<[u8]>,
    ) -> Self {
        Self {
            conversation_id,
            queues,
            services,
            state_machine,
            config,
            operating_mode: OperatingMode::Normal,
            recovery_rounds: 0,
            timing: Timing::new(),
            self_member_id,
            app_id,
            pending_events: Mutex::new(Vec::new()),
            pending_outbound: Mutex::new(Vec::new()),
        }
    }

    // ── Operating mode (Layer 3 Anti-Deadlock) ──────────────────────

    pub fn is_in_recovery_mode(&self) -> bool {
        self.operating_mode == OperatingMode::Recovery
    }

    pub fn enter_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Recovery;
        self.recovery_rounds = 0;
    }

    pub fn exit_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Normal;
    }

    // ── State accessor ──────────────────────────────────────────────

    pub fn current_state(&self) -> ConversationState {
        self.state_machine.current_state()
    }

    // ── MLS service ─────────────────────────────────────────────────

    /// Borrow the MLS service.
    pub(crate) fn mls(&self) -> &MlsService {
        &self.services.mls
    }

    /// Mutably borrow the MLS service — required for the commit pipeline
    /// and encrypt/decrypt methods that advance MLS state.
    pub(crate) fn mls_mut(&mut self) -> &mut MlsService {
        &mut self.services.mls
    }

    // ── Protocol-function wrappers ─────────────────────────────────
    //
    // Read `queues`, `mls`, and `steward` from `self` so coordinator
    // callsites don't destructure the conversation. Protocol logic lives in
    // sibling `core` modules; these are pure delegation.

    /// Build our own commit candidate from the approved proposals: mint the MLS
    /// commit, buffer it locally, and return the wire bytes to broadcast. The
    /// remote counterpart is `commit_round::receive_commit_candidate`.
    pub(crate) fn build_local_candidate<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        self_member_id: &[u8],
    ) -> Result<Option<Vec<u8>>, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if !self.services.steward_list.is_steward(self_member_id) && !self.is_in_recovery_mode() {
            return Err(ConversationError::NotASteward);
        }

        if self.queues.approved_proposals().is_empty() {
            return Err(ConversationError::NoProposals);
        }

        // MLS forbids committing one's own removal. If the approved batch contains
        // RemoveMember(self), skip local candidate creation — another steward will
        // commit the batch (including this node's removal) once they enter freeze.
        if self.queues.has_approved_removal(self_member_id) {
            info!(
                conversation = self.queues.name(),
                "commit candidate skipped: approved batch contains self-remove"
            );
            return Ok(None);
        }

        // Governance proposals (emergency, election) are consensus-only and must
        // not be in the approved queue at batch creation time.
        let non_mls_ids: Vec<u32> = self
            .queues
            .approved_proposals()
            .iter()
            .filter(|(_, req)| ProposalKind::of(req).is_governance())
            .map(|(&id, _)| id)
            .collect();

        if !non_mls_ids.is_empty() {
            return Err(ConversationError::UnexpectedNonMlsProposals {
                proposal_ids: non_mls_ids,
            });
        }

        // Borrow `self.services.mls` directly so later `self.queues` reads stay
        // a disjoint borrow.
        let mls = &mut self.services.mls;

        // Drop approved entries already reflected in conversation state (stale
        // rebroadcast KPs, duplicate removes) — without this MLS would reject
        // the whole batch with "Duplicate signature key in proposals and conversation".
        let current_members = mls.members()?;
        let current_members_set = member_set(&current_members);
        let is_member = |id: &[u8]| current_members_set.contains(id);

        // Urgent (ECP-driven) freeze: restrict the batch to just the target's
        // RemoveMember. See `ConversationQueues::urgent_commit_target`.
        let urgent_target = self.queues.urgent_commit_target().map(|t| t.to_vec());

        // Iterate in insertion order (FIFO): library proposal IDs are
        // content-derived hashes, so sort-by-id is not temporal.
        let k_max = self.config.commit_batch_max;
        let approved = self.queues.approved_proposals();
        let mut updates = Vec::with_capacity(approved.len().min(k_max));
        // Joiners admitted by this batch, in Add order. Travels with the
        // welcome so any holder can address delivery.
        let mut joiner_identities = Vec::new();
        for (_pid, proposal) in approved.iter().take(k_max) {
            match proposal.payload.as_ref() {
                Some(Payload::MemberInvite(im)) => {
                    if urgent_target.is_some() {
                        continue;
                    }
                    if is_member(&im.member_id) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Add(im.key_package_bytes.clone()));
                    joiner_identities.push(im.member_id.clone());
                }
                Some(Payload::RemoveMember(rm)) => {
                    if let Some(target) = urgent_target.as_deref()
                        && rm.member_id != target
                    {
                        continue;
                    }
                    if !is_member(&rm.member_id) {
                        continue;
                    }
                    updates.push(MlsCommitInput::Remove(rm.member_id.clone()));
                }
                _ => return Err(ConversationError::InvalidConversationUpdateRequest),
            }
        }

        if updates.is_empty() {
            return Ok(None);
        }

        let CommitArtifacts {
            proposals: mls_proposals,
            commit,
            welcome,
        } = mls.create_commit_candidate(provider, signer, &updates)?;

        let candidate = CommitCandidate {
            conversation_id: self.queues.name_bytes().to_vec(),
            mls_proposals,
            commit_message: commit,
            steward_member_id: self_member_id.to_vec(),
        };

        // Welcome bytes are buffered here but deferred until our merge, so
        // joiners can't advance epoch ahead of the steward.
        let epoch = mls.current_epoch()?;
        let max_candidates = mls.members()?.len();
        self.queues.commit_round.add(
            candidate.clone(),
            true,
            welcome,
            joiner_identities,
            epoch,
            max_candidates,
        );

        info!(
            conversation = self.queues.name(),
            epoch,
            proposals = updates.len(),
            "commit candidate created"
        );

        let candidate_msg: AppMessage = candidate.into();
        Ok(Some(candidate_msg.encode_to_vec()))
    }

    /// Finish entering `Freezing`: announce the phase change, and — if we're a
    /// steward — mint our own commit candidate and broadcast it to open the
    /// round. Non-stewards just announce and wait for a steward's candidate.
    /// `event` is the transition returned by `start_freezing` /
    /// `check_steward_inactivity`.
    pub(crate) fn on_freeze_entered<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        event: ConversationState,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        // Only stewards mint on freeze entry (non-stewards just enter the
        // round); the recovery-relaxed gate lives in `build_local_candidate`.
        if self.services.steward_list.is_steward(&self.self_member_id)
            && let Err(e) = self.mint_and_broadcast_candidate(provider, signer)
        {
            tracing::error!(
                conversation = %self.conversation_id,
                error = %e,
                "own commit candidate build failed"
            );
        }
        self.emit_event(ConversationEvent::PhaseChange(event));
        Ok(())
    }

    /// Mint this member's commit candidate from the approved batch and queue it
    /// for broadcast. `Ok(true)` if a candidate was produced, `Ok(false)` if
    /// there was nothing to commit (e.g. the batch is only our own removal).
    /// Shared by the freeze-entry mint, the manual recovery commit, and the
    /// recovery auto-fallback — the eligibility gate (stewards, or any member in
    /// recovery) lives in [`Self::build_local_candidate`].
    pub(crate) fn mint_and_broadcast_candidate<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<bool, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let self_member_id = Arc::clone(&self.self_member_id);
        match self.build_local_candidate(provider, signer, &self_member_id)? {
            Some(payload) => {
                self.broadcast(payload);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Manually produce a Layer-3 recovery commit: mint our candidate and queue
    /// its broadcast (only valid in recovery, where the steward gate is relaxed).
    /// `true` if a candidate was produced, `false` if there was nothing to commit.
    /// If no member calls this within `recovery_auto_commit_delay`, every online
    /// node auto-mints instead.
    pub fn commit_in_recovery<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<bool, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if !self.is_in_recovery_mode() {
            return Err(ConversationError::NotInRecovery);
        }
        self.mint_and_broadcast_candidate(provider, signer)
    }

    /// Finalize the active commit round.
    pub(crate) fn finalize_commit_round<Pr>(
        &mut self,
        provider: &Pr,
        allow_subset_candidates: bool,
        self_member_id: &[u8],
    ) -> Result<CommitRoundResult, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let in_recovery = self.operating_mode == OperatingMode::Recovery;
        let mls = &mut self.services.mls;
        finalize_commit_round(
            provider,
            &mut self.queues,
            mls,
            &self.services.steward_list,
            in_recovery,
            allow_subset_candidates,
            self_member_id,
        )
    }

    /// Re-buffer commit candidates stashed before their proposal was locally
    /// approved. Call after applying a consensus outcome. No-op when nothing
    /// is stashed.
    pub(crate) fn replay_early_candidates(&mut self) -> Result<(), ConversationError> {
        replay_early_candidates(&mut self.queues, &self.services.mls)
    }

    /// Decode an inbound app-subtopic payload into a [`ProcessResult`].
    pub(crate) fn decode_inbound<Pr>(
        &mut self,
        provider: &Pr,
        payload: &[u8],
    ) -> Result<ProcessResult, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        decode_inbound_payload(provider, &mut self.queues, &mut self.services.mls, payload)
    }

    /// Append a [`ConversationEvent`] to the pending-events buffer. The caller's
    /// polling cycle drains it via [`Self::drain_events`]. Stays `&self`
    /// thanks to the interior [`Mutex`], so the many coordinator
    /// methods that emit during a brief read guard don't need to escalate
    /// to a write guard. Fire-and-forget (no `Result`), but a poisoned
    /// buffer is logged rather than silently dropped.
    pub(crate) fn emit_event(&self, event: ConversationEvent) {
        match self.pending_events.lock() {
            Ok(mut buf) => buf.push(event),
            Err(_) => {
                tracing::error!(?event, "event buffer mutex poisoned; event dropped")
            }
        }
    }

    /// Drain every pending [`ConversationEvent`] accumulated since the last
    /// call. Returns events in insertion order. Callers (UI fanout,
    /// audit log) invoke this once per polling cycle.
    pub fn drain_events(&self) -> Vec<ConversationEvent> {
        match self.pending_events.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => {
                tracing::error!("event buffer mutex poisoned; UI fanout will miss events");
                Vec::new()
            }
        }
    }

    /// Smallest pending deadline relative to now, or `None` when nothing
    /// is scheduled. Returns `Some(Duration::ZERO)` for an already-elapsed
    /// deadline. Covers consensus-session timeouts, auto-vote timers, and
    /// state-machine phase deadlines (Freezing window, steward / recovery
    /// inactivity). Forward to an external scheduler that calls `poll()` on
    /// fire; extra/early wakeups are no-ops.
    pub fn next_wakeup_in(&self) -> Option<Duration> {
        let now = Instant::now();
        let earliest = self
            .timing
            .pending_consensus_timeouts
            .values()
            .copied()
            .chain(self.timing.pending_auto_votes.values().map(|e| e.fire_at))
            .chain(self.phase_deadline())
            .min()?;
        Some(earliest.saturating_duration_since(now))
    }

    /// State-driven phase-timer deadline, if one is currently active. The
    /// conversation's polling paths (the freeze step in `poll`, and the
    /// inactivity check in `check_steward_inactivity`) all gate on the phase
    /// timer; this surfaces the same wall-clock target so an external
    /// scheduler can wake us at the right time.
    fn phase_deadline(&self) -> Option<Instant> {
        let anchor = self.timing.phase_timer.started_at()?;
        let cfg = &self.config;
        match self.current_state() {
            ConversationState::Freezing => Some(anchor + cfg.freeze_duration),
            ConversationState::Working => {
                if self.queues.approved_proposals_count() == 0 {
                    return None;
                }
                let dur = if self.is_in_recovery_mode() {
                    cfg.recovery_inactivity_duration
                } else {
                    cfg.commit_inactivity_duration
                };
                Some(anchor + dur)
            }
            _ => None,
        }
    }

    /// Buffer encrypted conversation traffic (chat, votes, sync, commit
    /// candidates) as an [`Outbound`], stamped with this conversation and the
    /// local sender. Stays `&self` thanks to the interior `Mutex`, so send
    /// sites in `&self` methods don't escalate to a write guard. The conversation
    /// never sends — the caller drains via [`Self::drain_outbound`].
    pub(crate) fn broadcast(&self, payload: Vec<u8>) {
        let out = Outbound {
            conversation_id: self.conversation_id.clone(),
            sender: self.app_id.to_vec(),
            payload,
        };
        match self.pending_outbound.lock() {
            Ok(mut buf) => buf.push(out),
            Err(_) => {
                tracing::error!("outbound buffer mutex poisoned; item dropped")
            }
        }
    }

    /// Drain every buffered [`Outbound`] accumulated since the last call,
    /// in insertion order. The integrator invokes this once per cycle (after
    /// `poll` / `handle_inbound` / an intent) and maps each item onto its
    /// own transport.
    pub fn drain_outbound(&self) -> Vec<Outbound> {
        match self.pending_outbound.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => {
                tracing::error!("outbound buffer mutex poisoned; integrator will miss outbound");
                Vec::new()
            }
        }
    }

    // ── Pending deadlines (auto-votes + consensus timeouts) ─────────

    /// Register an auto-vote to fire `delay` from now with the given
    /// `vote` choice. Idempotent — re-registering for the same
    /// `proposal_id` replaces the existing entry.
    pub(crate) fn register_auto_vote(&mut self, proposal_id: u32, delay: Duration, vote: bool) {
        self.timing.pending_auto_votes.insert(
            proposal_id,
            AutoVoteEntry {
                fire_at: Instant::now() + delay,
                vote,
            },
        );
    }

    /// Drop the pending auto-vote for `proposal_id` if any is registered.
    /// Called when a manual vote arrives (manual choice wins) or when the
    /// consensus session resolves (vote no longer meaningful).
    pub(crate) fn cancel_auto_vote(&mut self, proposal_id: u32) {
        self.timing.pending_auto_votes.remove(&proposal_id);
    }

    /// Drop every pending auto-vote on this conversation. Called on every path that
    /// emits `Leaving` so no stale entries fire against a conversation we've
    /// left.
    pub(crate) fn cancel_all_auto_votes(&mut self) {
        self.timing.pending_auto_votes.clear();
    }

    /// Leave this conversation. Opens a self-leave consensus round and returns
    /// [`LeaveOutcome::LeaveInitiated`]; the leave completes when the next
    /// steward commit merges the removal. `signer` is the local member's MLS
    /// signer, used to authenticate the self-leave proposal.
    pub fn leave<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<LeaveOutcome, ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.initiate_self_leave(provider, signer)?;
        Ok(LeaveOutcome::LeaveInitiated)
    }

    /// Register a consensus-session timeout. Fires `delay` from now via
    /// `tick_deadlines`; removed naturally on consensus resolution.
    pub(crate) fn register_consensus_timeout(&mut self, proposal_id: u32, delay: Duration) {
        self.timing
            .pending_consensus_timeouts
            .insert(proposal_id, Instant::now() + delay);
    }

    /// Drop the pending consensus timeout for `proposal_id`. Called from
    /// `handle_consensus_outcome` once the library reaches/fails consensus,
    /// so the timeout can't fire a stale `handle_consensus_timeout` against
    /// an already-resolved consensus session.
    pub(crate) fn unregister_consensus_timeout(&mut self, proposal_id: u32) {
        self.timing.pending_consensus_timeouts.remove(&proposal_id);
    }

    // ── State-machine + phase-timer coordinators ────────────────────

    pub(crate) fn start_working(&mut self) -> ConversationState {
        self.state_machine.start_working();
        self.timing.phase_timer.clear();
        info!(state = "Working", "state transition");
        ConversationState::Working
    }

    /// Enter `Freezing` from `Working` or `Reelection`, starting the freeze
    /// phase timer. Returns `Some(Freezing)` on transition; `None` (no-op)
    /// from any other state.
    pub(crate) fn start_freezing(&mut self) -> Option<ConversationState> {
        if self.state_machine.start_freezing() {
            self.timing.phase_timer.start();
            info!(state = "Freezing", "state transition");
            Some(ConversationState::Freezing)
        } else {
            None
        }
    }

    pub(crate) fn start_selection(&mut self) -> ConversationState {
        self.state_machine.start_selection();
        info!(state = "Selection", "state transition");
        ConversationState::Selection
    }

    pub(crate) fn start_reelection(&mut self) -> ConversationState {
        self.state_machine.start_reelection();
        self.timing.phase_timer.clear();
        info!(state = "Reelection", "state transition");
        ConversationState::Reelection
    }

    /// Re-open the Layer-3 recovery collection window: `Selection` → `Freezing`
    /// with the phase timer re-anchored, so the next poll re-enters
    /// `advance_freezing` and the manual/auto commit cycle retries. Returns
    /// `Some(Freezing)` on transition; `None` from any other state.
    pub(crate) fn reopen_recovery_window(&mut self) -> Option<ConversationState> {
        if self.state_machine.reopen_freezing() {
            self.timing.phase_timer.start();
            info!(state = "Freezing", "state transition (recovery retry)");
            Some(ConversationState::Freezing)
        } else {
            None
        }
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub(crate) fn is_freeze_window_elapsed(&self) -> bool {
        self.current_state() == ConversationState::Freezing
            && self
                .timing
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
    ) -> Option<ConversationState> {
        if self.current_state() != ConversationState::Working || approved_proposals_count == 0 {
            return None;
        }
        if self.timing.phase_timer.started_at().is_none() {
            self.timing.phase_timer.start();
            info!(
                approved = approved_proposals_count,
                inactivity_ms = inactivity_duration.as_millis() as u64,
                "inactivity timer started"
            );
            return None;
        }
        if !self
            .timing
            .phase_timer
            .elapsed_since_anchor(inactivity_duration)
        {
            return None;
        }
        info!(
            inactivity_ms = inactivity_duration.as_millis() as u64,
            approved = approved_proposals_count,
            "inactivity window elapsed, entering freeze"
        );
        self.start_freezing()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use openmls_basic_credential::SignatureKeyPair;

    use super::*;
    use crate::ConversationQueues;
    use crate::defaults::DefaultConsensusPlugin;
    use crate::defaults::InMemoryPeerScoreStorage;
    use crate::test_fixtures::{
        TestMls, TestProvider, make_creator_mls, make_test_consensus_service,
        steward_service_member, steward_service_steward,
    };
    use crate::{PeerScoringService, ScoringConfig, StewardListService};
    use std::collections::HashMap;

    type TestConversation = Conversation<DefaultConsensusPlugin, InMemoryPeerScoreStorage>;

    /// Build a conversation with a real creator-side MLS service and the given
    /// steward roster, returning it alongside the provider that backs the MLS
    /// group and the signer that seeded it (for paths that sign — the guard
    /// tests early-return before then).
    fn make_conversation_with_steward(
        steward_list: StewardListService,
    ) -> (TestConversation, TestProvider, SignatureKeyPair) {
        let (mls, provider, signer) = make_creator_mls(b"test-member-id");
        (build_conversation(mls, steward_list), provider, signer)
    }

    fn make_conversation_working() -> TestConversation {
        let (mls, _provider, _signer) = make_creator_mls(b"test-member-id");
        build_conversation(mls, steward_service_member())
    }

    /// Seed one approved proposal so `has_proposals` is true, keeping a
    /// commit-less recovery round on the retry path rather than the empty-exit.
    fn seed_approved_work(conversation: &mut TestConversation) {
        use crate::protos::de_mls::messages::v1::ViolationEvidence;
        let req = ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new())
            .with_creator(vec![0x01])
            .into_update_request()
            .unwrap();
        conversation.queues.insert_approved_proposal(1, req);
    }

    fn build_conversation(mls: TestMls, steward_list: StewardListService) -> TestConversation {
        build_conversation_scored::<InMemoryPeerScoreStorage>(mls, steward_list)
    }

    fn build_conversation_scored<Sc: PeerScoreStorage + Default>(
        mls: TestMls,
        steward_list: StewardListService,
    ) -> Conversation<DefaultConsensusPlugin, Sc> {
        let (consensus, consensus_rx) = make_test_consensus_service();
        Conversation::new(
            "g".to_string(),
            ConversationQueues::new("g", 10),
            ConversationServices {
                mls,
                scoring: PeerScoringService::new(
                    Sc::default(),
                    HashMap::new(),
                    ScoringConfig::default(),
                ),
                steward_list,
                consensus,
                consensus_rx,
            },
            ConversationStateMachine::new_as_member(),
            ConversationConfig::default(),
            Arc::from(&b"test-member-id"[..]),
            Arc::from(&[0u8; 16][..]),
        )
    }

    /// A [`PeerScoreStorage`] whose every read/write fails — for asserting that
    /// post-commit scoring errors don't wedge the state machine.
    #[derive(Debug)]
    struct ScoringFault;
    impl std::fmt::Display for ScoringFault {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "scoring storage fault")
        }
    }
    impl std::error::Error for ScoringFault {}

    #[derive(Default)]
    struct FailingScoreStorage;
    impl PeerScoreStorage for FailingScoreStorage {
        type Error = ScoringFault;
        fn get(&self, _member_id: &[u8]) -> Result<Option<i64>, Self::Error> {
            Err(ScoringFault)
        }
        fn set(&mut self, _member_id: &[u8], _score: i64) -> Result<(), Self::Error> {
            Err(ScoringFault)
        }
        fn remove(&mut self, _member_id: &[u8]) -> Result<(), Self::Error> {
            Err(ScoringFault)
        }
        fn all_scores(&self) -> Result<Vec<(Vec<u8>, i64)>, Self::Error> {
            Err(ScoringFault)
        }
    }

    /// First tick with approved work auto-anchors the timer and returns `None`.
    /// Second tick before timeout still returns `None`. State must remain Working.
    #[test]
    fn check_steward_inactivity_first_tick_anchors_and_returns_none() {
        let mut conversation = make_conversation_working();
        assert_eq!(conversation.current_state(), ConversationState::Working);
        assert!(
            conversation.timing.phase_timer.started_at().is_none(),
            "fresh conversation has no anchor"
        );

        let result =
            conversation.check_steward_inactivity(/* approved */ 1, Duration::from_secs(10));

        assert_eq!(result, None, "first tick auto-anchors and returns None");
        assert!(
            conversation.timing.phase_timer.started_at().is_some(),
            "anchor must be set after first tick"
        );
        assert_eq!(
            conversation.current_state(),
            ConversationState::Working,
            "state must stay Working until inactivity actually elapses"
        );

        let result =
            conversation.check_steward_inactivity(/* approved */ 1, Duration::from_secs(10));
        assert_eq!(
            result, None,
            "second tick before timeout still returns None"
        );
    }

    /// No approved work → no anchor started, no transition.
    #[test]
    fn check_steward_inactivity_noop_without_approved_work() {
        let mut conversation = make_conversation_working();
        let result = conversation.check_steward_inactivity(0, Duration::from_secs(10));
        assert_eq!(result, None);
        assert!(
            conversation.timing.phase_timer.started_at().is_none(),
            "no approved work must not start the timer"
        );
    }

    // ── Caller-polled deadlines + drain model ───────────────────────────

    /// `emit_event` appends and `drain_events` returns insertion-ordered.
    /// Establishes the contract relied on by integration tests that build
    /// up an event log over multiple polling cycles.
    #[test]
    fn emit_event_then_drain_returns_insertion_order_and_clears_buffer() {
        let conversation = make_conversation_working();
        conversation.emit_event(ConversationEvent::PhaseChange(ConversationState::Working));
        conversation.emit_event(ConversationEvent::Leaving);

        let drained = conversation.drain_events();
        assert_eq!(drained.len(), 2);
        assert!(matches!(
            drained[0],
            ConversationEvent::PhaseChange(ConversationState::Working)
        ));
        assert!(matches!(drained[1], ConversationEvent::Leaving));

        // Second drain returns empty — buffer was cleared.
        assert!(conversation.drain_events().is_empty());
    }

    /// `register_auto_vote` is idempotent — re-registering the same
    /// `proposal_id` replaces the previous entry rather than stacking.
    /// Caller relies on this when re-anchoring an auto-vote on a `Deferred`
    /// re-submit.
    #[test]
    fn register_auto_vote_replaces_existing_entry() {
        let mut conversation = make_conversation_working();
        conversation.register_auto_vote(7, Duration::from_secs(10), true);
        let first_fire = conversation.timing.pending_auto_votes[&7].fire_at;

        // Re-register with a different `vote` and a longer delay; the
        // second insert must overwrite, not co-exist.
        std::thread::sleep(Duration::from_millis(2));
        conversation.register_auto_vote(7, Duration::from_secs(20), false);
        assert_eq!(conversation.timing.pending_auto_votes.len(), 1);
        let entry = conversation.timing.pending_auto_votes[&7];
        assert!(!entry.vote);
        assert!(entry.fire_at > first_fire);
    }

    /// `cancel_auto_vote` drops one entry. `cancel_all_auto_votes` drops
    /// every entry. Both are the only paths that should remove pending
    /// auto-votes from outside `tick_deadlines`.
    #[test]
    fn cancel_auto_vote_removes_only_the_targeted_proposal() {
        let mut conversation = make_conversation_working();
        conversation.register_auto_vote(1, Duration::from_secs(5), true);
        conversation.register_auto_vote(2, Duration::from_secs(5), false);
        conversation.register_auto_vote(3, Duration::from_secs(5), true);

        conversation.cancel_auto_vote(2);
        assert!(conversation.timing.pending_auto_votes.contains_key(&1));
        assert!(!conversation.timing.pending_auto_votes.contains_key(&2));
        assert!(conversation.timing.pending_auto_votes.contains_key(&3));

        conversation.cancel_all_auto_votes();
        assert!(conversation.timing.pending_auto_votes.is_empty());
    }

    /// `register_consensus_timeout` records `now + delay`;
    /// `unregister_consensus_timeout` drops it. `handle_consensus_outcome`
    /// uses the unregister path to drop deadlines on natural resolution
    /// so `tick_deadlines` doesn't fire a stale `handle_consensus_timeout`.
    #[test]
    fn register_then_unregister_consensus_timeout() {
        let mut conversation = make_conversation_working();
        let before = Instant::now();
        conversation.register_consensus_timeout(42, Duration::from_secs(30));
        let fire_at = conversation.timing.pending_consensus_timeouts[&42];
        assert!(fire_at > before + Duration::from_secs(29));
        assert!(fire_at < Instant::now() + Duration::from_secs(31));

        conversation.unregister_consensus_timeout(42);
        assert!(
            !conversation
                .timing
                .pending_consensus_timeouts
                .contains_key(&42)
        );

        // Unregistering an unknown id is a no-op (no panic, no error).
        conversation.unregister_consensus_timeout(999);
    }

    // ── build_local_candidate guards ──────────────────────────────────

    #[test]
    fn build_local_candidate_errors_for_non_steward_outside_recovery() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_member());
        let err = conversation
            .build_local_candidate(&provider, &signer, b"me")
            .expect_err("non-steward should be rejected");
        assert!(matches!(err, ConversationError::NotASteward));
    }

    #[test]
    fn build_local_candidate_errors_when_no_approved_proposals() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_steward(b"me"));
        let err = conversation
            .build_local_candidate(&provider, &signer, b"me")
            .expect_err("empty approved queue should be rejected");
        assert!(matches!(err, ConversationError::NoProposals));
    }

    /// An emergency-criteria proposal in the approved queue must surface as
    /// `UnexpectedNonMlsProposals` — only MLS-producing payloads belong in a
    /// commit. The error carries the offending proposal ids so the
    /// orchestrator can drop them.
    #[test]
    fn build_local_candidate_errors_on_emergency_in_approved_queue() {
        use crate::protos::de_mls::messages::v1::ViolationEvidence;

        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_steward(b"me"));
        let emergency = ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new())
            .with_creator(vec![0x01])
            .into_update_request()
            .unwrap();
        conversation.queues.insert_approved_proposal(50, emergency);

        let err = conversation
            .build_local_candidate(&provider, &signer, b"me")
            .expect_err("emergency in approved queue should be rejected");
        let ConversationError::UnexpectedNonMlsProposals { proposal_ids } = err else {
            panic!("expected UnexpectedNonMlsProposals, got {err:?}");
        };
        assert_eq!(proposal_ids, vec![50]);
    }

    #[test]
    fn commit_in_recovery_errors_outside_recovery() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_member());
        let err = conversation
            .commit_in_recovery(&provider, &signer)
            .expect_err("commit_in_recovery outside recovery must be rejected");
        assert!(matches!(err, ConversationError::NotInRecovery));
    }

    #[test]
    fn commit_in_recovery_relaxes_the_steward_gate() {
        // A non-steward: outside recovery it can't mint (`NotASteward`). In
        // recovery the gate is relaxed, so the build gets past it and only stops
        // at the empty approved queue — proving any member may commit in recovery.
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_member());
        conversation.enter_recovery_mode();
        let err = conversation
            .commit_in_recovery(&provider, &signer)
            .expect_err("empty approved queue should stop the build");
        assert!(
            matches!(err, ConversationError::NoProposals),
            "recovery must relax the steward gate, got {err:?}"
        );
    }

    #[test]
    fn merged_commit_reaches_working_even_if_scoring_storage_fails() {
        let (mls, provider, signer) = make_creator_mls(b"test-member-id");
        let mut conversation =
            build_conversation_scored::<FailingScoreStorage>(mls, steward_service_member());
        // Mid commit-round, as after a merge lands.
        conversation.start_selection();

        // A merged commit dispatches ConversationUpdated. The post-commit scoring
        // sync fails here, but the transition must still land — no Selection wedge.
        let _ = conversation.dispatch_inbound_result(
            &provider,
            ProcessResult::ConversationUpdated,
            &signer,
        );

        assert_eq!(
            conversation.current_state(),
            ConversationState::Working,
            "a merged commit reaches Working even when post-commit bookkeeping fails"
        );
    }

    #[test]
    fn recovery_auto_commit_waits_out_the_grace() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_member());
        // A grace long enough that it won't elapse during the test.
        conversation.config.recovery_auto_commit_delay = Some(Duration::from_secs(3600));
        conversation.config.recovery_inactivity_duration = Duration::ZERO;
        conversation.enter_recovery_mode();
        conversation.start_freezing();
        conversation.timing.phase_timer.start();
        seed_approved_work(&mut conversation);

        conversation.poll(&provider, &signer);

        assert!(
            !conversation.queues.commit_round.has_local_candidate(),
            "no auto-mint before the grace elapses"
        );
        assert!(conversation.is_in_recovery_mode());
    }

    #[test]
    fn is_deadlock_proposer_true_for_sole_steward() {
        let conversation =
            make_conversation_with_steward(steward_service_steward(b"test-member-id")).0;
        assert!(
            conversation.is_deadlock_proposer().unwrap(),
            "the sole steward is the deterministic deadlock proposer"
        );
    }

    #[test]
    fn request_recovery_is_noop_while_in_recovery() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_steward(b"test-member-id"));
        conversation.enter_recovery_mode();
        conversation.request_recovery(&provider, &signer).unwrap();
        assert!(
            conversation.drain_outbound().is_empty(),
            "no Deadlock ECP filed while already in recovery"
        );
    }

    #[test]
    fn recovery_with_empty_queue_exits_without_spinning() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_member());
        conversation.config.recovery_auto_commit_delay = Some(Duration::ZERO);
        conversation.config.recovery_inactivity_duration = Duration::ZERO;
        // 0 would spin forever without the empty-queue exit.
        conversation.config.recovery_max_rounds = 0;
        conversation.enter_recovery_mode();
        conversation.start_freezing();
        conversation.timing.phase_timer.start();

        // No approved proposals — nothing to recover.
        conversation.poll(&provider, &signer);

        assert!(
            !conversation.is_in_recovery_mode(),
            "recovery with an empty queue must exit, not spin"
        );
        assert_eq!(conversation.current_state(), ConversationState::Working);
        assert!(
            !conversation
                .drain_events()
                .iter()
                .any(|e| matches!(e, ConversationEvent::RecoveryExhausted)),
            "an empty-queue exit is not an exhaustion"
        );
    }

    #[test]
    fn recovery_retries_across_rounds_until_stop_line() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_member());
        // Zero-length windows so each poll resolves one commit-less round.
        // Manual-only (no auto-mint) with real pending work: rounds produce no
        // commit and stay on the retry path.
        conversation.config.recovery_auto_commit_delay = None;
        conversation.config.recovery_inactivity_duration = Duration::ZERO;
        conversation.config.recovery_max_rounds = 3;
        conversation.enter_recovery_mode();
        conversation.start_freezing();
        conversation.timing.phase_timer.start();
        seed_approved_work(&mut conversation);

        // Rounds 1 and 2 land no commit but must re-open the collection window
        // (Freezing) rather than wedging in Selection.
        for round in 1..=2 {
            conversation.poll(&provider, &signer);
            assert!(
                conversation.is_in_recovery_mode(),
                "round {round}: recovery must stay open below the stop line"
            );
            assert_eq!(
                conversation.current_state(),
                ConversationState::Freezing,
                "round {round}: recovery must re-open Freezing, not wedge in Selection"
            );
        }

        // Round 3 hits recovery_max_rounds → exhausts back to Working.
        conversation.poll(&provider, &signer);
        assert!(
            !conversation.is_in_recovery_mode(),
            "recovery must exit at the stop line"
        );
        assert_eq!(conversation.current_state(), ConversationState::Working);
        assert!(
            conversation
                .drain_events()
                .iter()
                .any(|e| matches!(e, ConversationEvent::RecoveryExhausted)),
            "RecoveryExhausted must fire at the stop line"
        );
    }

    #[test]
    fn recovery_retries_forever_when_max_rounds_zero() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_member());
        conversation.config.recovery_auto_commit_delay = None;
        conversation.config.recovery_inactivity_duration = Duration::ZERO;
        conversation.config.recovery_max_rounds = 0; // retry forever (default)
        conversation.enter_recovery_mode();
        conversation.start_freezing();
        conversation.timing.phase_timer.start();
        seed_approved_work(&mut conversation);

        for round in 1..=5 {
            conversation.poll(&provider, &signer);
            assert!(
                conversation.is_in_recovery_mode(),
                "round {round}: recovery must stay open when max_rounds is 0"
            );
            assert_eq!(
                conversation.current_state(),
                ConversationState::Freezing,
                "round {round}: recovery must re-open Freezing, not wedge in Selection"
            );
            assert!(
                !conversation
                    .drain_events()
                    .iter()
                    .any(|e| matches!(e, ConversationEvent::RecoveryExhausted)),
                "round {round}: RecoveryExhausted must never fire when max_rounds is 0"
            );
        }
    }

    #[test]
    fn recovery_exhausts_after_max_rounds() {
        let (mut conversation, provider, signer) =
            make_conversation_with_steward(steward_service_member());
        // Manual-only with pending work and a 1-round stop line: the first
        // commit-less round must exhaust recovery.
        conversation.config.recovery_auto_commit_delay = None;
        conversation.config.recovery_inactivity_duration = Duration::ZERO;
        conversation.config.recovery_max_rounds = 1;
        conversation.enter_recovery_mode();
        conversation.start_freezing();
        conversation.timing.phase_timer.start();
        seed_approved_work(&mut conversation);

        conversation.poll(&provider, &signer);

        assert!(
            !conversation.is_in_recovery_mode(),
            "recovery must exit at the stop line"
        );
        assert_eq!(conversation.current_state(), ConversationState::Working);
        assert!(
            conversation
                .drain_events()
                .iter()
                .any(|e| matches!(e, ConversationEvent::RecoveryExhausted)),
            "RecoveryExhausted must be emitted"
        );
    }
}
