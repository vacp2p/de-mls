//! [`Conversation`] struct, constructor, and the state-machine + phase-timer
//! coordinators that hold per-conversation protocol state, the MLS service,
//! plug-ins, and durable config alongside [`crate::PhaseTimer`] under
//! one lock. Per-conversation method bodies (proposal submission, voting,
//! inbound dispatch, etc.) live in sibling modules and extend `Conversation`
//! via additional `impl` blocks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use openmls_traits::signatures::Signer;
use prost::Message;
use tracing::info;

use hashgraph_like_consensus::events::ConsensusEventBus;

use crate::{
    BufferedCommitCandidate, ConsensusPlugin, ConsensusServiceFor, ConversationConfig,
    ConversationError, ConversationEvent, ConversationPlugins, ConversationQueues,
    ConversationState, ConversationStateMachine, FreezeBufferOutcome, FreezeFinalizeResult,
    OperatingMode, Outbound, PhaseTimer, ProcessResult, ProposalKind, StewardListPlugin,
    compute_commit_hash, decode_inbound_payload, finalize_freeze_round, member_set,
    mls_crypto::{
        CommitCandidate as MlsCommitCandidate, KeyPackageBytes, MlsCommitInput, MlsService,
    },
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

/// Receiver type the conversation drains from `tick_deadlines`. Resolves to the
/// `Receiver` associated type on the plugin's [`ConsensusEventBus`], which
/// is bound to implement [`crate::SyncConsensusReceiver`].
pub(crate) type ConsensusReceiver<P> = <<P as ConsensusPlugin>::EventBus as ConsensusEventBus<
    <P as ConsensusPlugin>::Scope,
>>::Receiver;

/// One pending auto-vote: cast `vote` for `proposal_id` once the wall-clock
/// catches up to `fire_at`. Registered by `initiate_proposal` (Deferred
/// path) and `on_incoming_proposal`; cancelled on manual vote or consensus
/// resolution; fired by [`Conversation::tick_deadlines`].
#[derive(Debug, Clone, Copy)]
pub struct AutoVoteEntry {
    pub fire_at: Instant,
    pub vote: bool,
}

pub struct Conversation<P: ConsensusPlugin, CP: ConversationPlugins> {
    /// Conversation name. Identifies this conversation in the integrator's
    /// registry and is used to construct scope keys for consensus operations.
    /// Read via [`Conversation::conversation_id`].
    pub(crate) conversation_id: String,
    pub(crate) queues: ConversationQueues,
    /// Per-conversation MLS service. Present for the conversation's whole
    /// lifetime: the creator seeds it at [`Conversation::create`], the joiner
    /// at [`Conversation::join`].
    mls: CP::Mls,
    pub(crate) state_machine: ConversationStateMachine,
    /// Per-conversation durable config: voting/consensus durations,
    /// `liveness_criteria_yes`, `pending_update_max_epochs`.
    pub(crate) config: ConversationConfig,
    /// Per-conversation peer-score plug-in.
    pub(crate) scoring: CP::Scoring,
    /// Per-conversation steward-list plug-in.
    pub(crate) steward_list: CP::StewardList,
    /// Authorization mode (RFC §Layer 3 Anti-Deadlock ECP).
    operating_mode: OperatingMode,
    /// Per-conversation consensus service. Owns this conversation's scope
    /// in the shared storage and a private event bus. Minted from the
    /// [`crate::ConversationDeps`] consensus service at construction.
    pub(crate) consensus: ConsensusServiceFor<P>,
    /// Subscriber on `consensus.event_bus()`. Drained by
    /// `tick_deadlines`, which dispatches each event through
    /// `apply_consensus_outcome`. Subscribed when the conversation is built in
    /// [`Conversation::create`] / [`Conversation::join`].
    pub(crate) consensus_rx: ConsensusReceiver<P>,
    /// Wall-clock anchor combined with [`Self::state_machine`] by
    /// coordinator methods.
    phase_timer: PhaseTimer,
    /// Pending auto-votes by `proposal_id`. Walked by
    /// `tick_deadlines`; each entry whose `fire_at` has passed
    /// gets a `cast_vote` and is removed from the map. Cancelled (removed)
    /// when a manual vote arrives or the consensus session resolves.
    pub(crate) pending_auto_votes: HashMap<u32, AutoVoteEntry>,
    /// Pending consensus-session timeouts: `proposal_id -> fire_at`.
    /// Registered when a proposal opens (own or incoming peer); fired by
    /// `tick_deadlines` which calls `consensus.handle_consensus_timeout`.
    /// Removed when the consensus session resolves naturally via `apply_consensus_outcome`.
    pub(crate) pending_consensus_timeouts: HashMap<u32, Instant>,
    /// Identity bytes of the local member, snapshotted from the `member_id`
    /// passed at construction. Read via [`Conversation::member_id_bytes`].
    pub(crate) self_member_id: Arc<[u8]>,
    /// Display form of the local member id, derived at construction.
    /// `Arc<str>` for the same reason as `self_member_id` — cheap clone
    /// across guard boundaries. Read via [`Conversation::member_id_display`].
    pub(crate) member_id_display: Arc<str>,
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
    /// Last freeze-progress snapshot emitted as `ConversationEvent::FreezeProgress`.
    /// `poll()` compares the current `(received, expected)` against this and
    /// emits a new event only when the count changes, avoiding repeated events
    /// on consecutive polling ticks that observe the same progress. Reset to
    /// `None` when the conversation leaves `Freezing`.
    pub(crate) last_freeze_progress: Option<(usize, usize)>,
}

impl<P: ConsensusPlugin, CP: ConversationPlugins> Conversation<P, CP> {
    /// Build a fresh conversation around an already-seeded MLS service.
    /// `consensus_rx` is a subscriber on `consensus.event_bus()`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        conversation_id: String,
        queues: ConversationQueues,
        mls: CP::Mls,
        state_machine: ConversationStateMachine,
        phase_timer: PhaseTimer,
        config: ConversationConfig,
        scoring: CP::Scoring,
        steward_list: CP::StewardList,
        consensus: ConsensusServiceFor<P>,
        consensus_rx: ConsensusReceiver<P>,
        self_member_id: Arc<[u8]>,
        member_id_display: Arc<str>,
        app_id: Arc<[u8]>,
    ) -> Self {
        Self {
            conversation_id,
            queues,
            mls,
            state_machine,
            config,
            scoring,
            steward_list,
            operating_mode: OperatingMode::Normal,
            consensus,
            consensus_rx,
            phase_timer,
            pending_auto_votes: HashMap::new(),
            pending_consensus_timeouts: HashMap::new(),
            self_member_id,
            member_id_display,
            app_id,
            pending_events: Mutex::new(Vec::new()),
            pending_outbound: Mutex::new(Vec::new()),
            last_freeze_progress: None,
        }
    }

    // ── Operating mode (Layer 3 Anti-Deadlock) ──────────────────────

    pub fn is_in_recovery_mode(&self) -> bool {
        self.operating_mode == OperatingMode::Recovery
    }

    pub fn enter_recovery_mode(&mut self) {
        self.operating_mode = OperatingMode::Recovery;
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
    pub(crate) fn mls(&self) -> &CP::Mls {
        &self.mls
    }

    /// Mutably borrow the MLS service — required for the commit pipeline
    /// and encrypt/decrypt methods that advance MLS state.
    pub(crate) fn mls_mut(&mut self) -> &mut CP::Mls {
        &mut self.mls
    }

    // ── Protocol-function wrappers ─────────────────────────────────
    //
    // Read `queues`, `mls`, and `steward` from `self` so coordinator
    // callsites don't destructure the conversation. Protocol logic lives in
    // sibling `core` modules; these are pure delegation.

    /// Build a commit candidate.
    pub(crate) fn create_commit_candidate(
        &mut self,
        signer: &impl Signer,
        self_member_id: &[u8],
    ) -> Result<Option<Vec<u8>>, ConversationError> {
        if !self.steward_list.is_steward(self_member_id) && !self.is_in_recovery_mode() {
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

        // Borrow `self.mls` directly so later `self.queues` reads stay
        // a disjoint borrow.
        let mls = &mut self.mls;

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
        let k_max = mls.commit_batch_max();
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
                    updates.push(MlsCommitInput::Add(KeyPackageBytes::new(
                        im.key_package_bytes.clone(),
                        im.member_id.clone(),
                    )));
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

        let MlsCommitCandidate {
            proposals: mls_proposals,
            commit,
            welcome,
        } = mls.create_commit_candidate(signer, &updates)?;

        let candidate = CommitCandidate {
            conversation_id: self.queues.name_bytes().to_vec(),
            mls_proposals,
            commit_message: commit,
            steward_member_id: self_member_id.to_vec(),
        };

        // Welcome bytes are deferred until our merge so joiners can't
        // advance epoch ahead of the steward.
        let commit_hash = compute_commit_hash(&candidate.commit_message);
        let epoch = mls.current_epoch()?;
        let max_candidates = mls.members()?.len();
        let outcome = self.queues.add_freeze_candidate(
            BufferedCommitCandidate {
                candidate_msg: candidate.clone(),
                commit_hash,
                is_local_candidate: true,
                welcome_bytes: welcome,
                joiner_identities,
            },
            epoch,
            max_candidates,
        );
        // Non-Buffered outcomes are legitimate runtime states (see
        // `FreezeBufferOutcome`), not errors — log at debug.
        if !matches!(outcome, FreezeBufferOutcome::Buffered) {
            tracing::debug!(
                conversation = self.queues.name(),
                epoch,
                ?outcome,
                "local commit candidate not buffered",
            );
        }

        info!(
            conversation = self.queues.name(),
            epoch,
            proposals = updates.len(),
            "commit candidate created"
        );

        let candidate_msg: AppMessage = candidate.into();
        Ok(Some(candidate_msg.encode_to_vec()))
    }

    /// Finalize the active freeze round.
    pub(crate) fn finalize_freeze_round(
        &mut self,
        allow_subset_candidates: bool,
        self_member_id: &[u8],
    ) -> Result<FreezeFinalizeResult, ConversationError> {
        let in_recovery = self.operating_mode == OperatingMode::Recovery;
        let mls = &mut self.mls;
        finalize_freeze_round(
            &mut self.queues,
            mls,
            &self.steward_list,
            in_recovery,
            allow_subset_candidates,
            self_member_id,
        )
    }

    /// Re-buffer commit candidates stashed before their proposal was locally
    /// approved. Call after applying a consensus outcome. No-op when nothing
    /// is stashed.
    pub(crate) fn replay_early_candidates(&mut self) -> Result<(), ConversationError> {
        replay_early_candidates(&mut self.queues, &mut self.mls)
    }

    /// Decode an inbound app-subtopic payload into a [`ProcessResult`].
    pub(crate) fn decode_inbound(
        &mut self,
        payload: &[u8],
    ) -> Result<ProcessResult, ConversationError> {
        decode_inbound_payload(&mut self.queues, &mut self.mls, payload)
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
            .pending_consensus_timeouts
            .values()
            .copied()
            .chain(self.pending_auto_votes.values().map(|e| e.fire_at))
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
        let anchor = self.phase_timer.started_at()?;
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
        self.pending_auto_votes.insert(
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
        self.pending_auto_votes.remove(&proposal_id);
    }

    /// Drop every pending auto-vote on this conversation. Called on every path that
    /// emits `Leaving` so no stale entries fire against a conversation we've
    /// left.
    pub(crate) fn cancel_all_auto_votes(&mut self) {
        self.pending_auto_votes.clear();
    }

    /// Leave this conversation. Opens a self-leave consensus round and returns
    /// [`LeaveOutcome::LeaveInitiated`]; the leave completes when the next
    /// steward commit merges the removal. `signer` is the local member's MLS
    /// signer, used to authenticate the self-leave proposal.
    pub fn leave(&mut self, signer: &impl Signer) -> Result<LeaveOutcome, ConversationError> {
        self.initiate_self_leave(signer)?;
        Ok(LeaveOutcome::LeaveInitiated)
    }

    /// Register a consensus-session timeout. Fires `delay` from now via
    /// `tick_deadlines`; removed naturally on consensus resolution.
    pub(crate) fn register_consensus_timeout(&mut self, proposal_id: u32, delay: Duration) {
        self.pending_consensus_timeouts
            .insert(proposal_id, Instant::now() + delay);
    }

    /// Drop the pending consensus timeout for `proposal_id`. Called from
    /// `apply_consensus_outcome` once the library reaches/fails consensus,
    /// so the timeout can't fire a stale `handle_consensus_timeout` against
    /// an already-resolved consensus session.
    pub(crate) fn unregister_consensus_timeout(&mut self, proposal_id: u32) {
        self.pending_consensus_timeouts.remove(&proposal_id);
    }

    // ── State-machine + phase-timer coordinators ────────────────────

    pub(crate) fn start_working(&mut self) -> ConversationState {
        self.state_machine.start_working();
        self.phase_timer.clear();
        info!(state = "Working", "state transition");
        ConversationState::Working
    }

    /// Enter `Freezing` from `Working` or `Reelection`, starting the freeze
    /// phase timer. Returns `Some(Freezing)` on transition; `None` (no-op)
    /// from any other state.
    pub(crate) fn start_freezing(&mut self) -> Option<ConversationState> {
        if self.state_machine.start_freezing() {
            self.phase_timer.start();
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
        self.phase_timer.clear();
        info!(state = "Reelection", "state transition");
        ConversationState::Reelection
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub(crate) fn is_freeze_timed_out(&self) -> bool {
        self.current_state() == ConversationState::Freezing
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
    ) -> Option<ConversationState> {
        if self.current_state() != ConversationState::Working || approved_proposals_count == 0 {
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
        self.start_freezing()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::ConversationQueues;
    use crate::defaults::DefaultConsensusPlugin;
    use crate::test_fixtures::{
        StubPlugins, StubScoring, StubStewardList, UnusedMls, UnusedSigner,
        make_test_consensus_service,
    };

    fn make_conversation_with_steward(
        steward_list: StubStewardList,
    ) -> Conversation<DefaultConsensusPlugin, StubPlugins> {
        let (consensus, consensus_rx) = make_test_consensus_service();
        Conversation::new(
            "g".to_string(),
            ConversationQueues::new("g"),
            UnusedMls,
            ConversationStateMachine::new_as_member(),
            PhaseTimer::new(),
            ConversationConfig::default(),
            StubScoring,
            steward_list,
            consensus,
            consensus_rx,
            Arc::from(&b"test-member-id"[..]),
            Arc::from("0xtest-display"),
            Arc::from(&[0u8; 16][..]),
        )
    }

    fn make_conversation_working() -> Conversation<DefaultConsensusPlugin, StubPlugins> {
        let (consensus, consensus_rx) = make_test_consensus_service();
        Conversation::new(
            "g".to_string(),
            ConversationQueues::new("g"),
            UnusedMls,
            ConversationStateMachine::new_as_member(),
            PhaseTimer::new(),
            ConversationConfig::default(),
            StubScoring,
            StubStewardList::member(),
            consensus,
            consensus_rx,
            Arc::from(&b"test-member-id"[..]),
            Arc::from("0xtest-display"),
            Arc::from(&[0u8; 16][..]),
        )
    }

    /// First tick with approved work auto-anchors the timer and returns `None`.
    /// Second tick before timeout still returns `None`. State must remain Working.
    #[test]
    fn check_steward_inactivity_first_tick_anchors_and_returns_none() {
        let mut conversation = make_conversation_working();
        assert_eq!(conversation.current_state(), ConversationState::Working);
        assert!(
            conversation.phase_timer.started_at().is_none(),
            "fresh conversation has no anchor"
        );

        let result =
            conversation.check_steward_inactivity(/* approved */ 1, Duration::from_secs(10));

        assert_eq!(result, None, "first tick auto-anchors and returns None");
        assert!(
            conversation.phase_timer.started_at().is_some(),
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
            conversation.phase_timer.started_at().is_none(),
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
        let first_fire = conversation.pending_auto_votes[&7].fire_at;

        // Re-register with a different `vote` and a longer delay; the
        // second insert must overwrite, not co-exist.
        std::thread::sleep(Duration::from_millis(2));
        conversation.register_auto_vote(7, Duration::from_secs(20), false);
        assert_eq!(conversation.pending_auto_votes.len(), 1);
        let entry = conversation.pending_auto_votes[&7];
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
        assert!(conversation.pending_auto_votes.contains_key(&1));
        assert!(!conversation.pending_auto_votes.contains_key(&2));
        assert!(conversation.pending_auto_votes.contains_key(&3));

        conversation.cancel_all_auto_votes();
        assert!(conversation.pending_auto_votes.is_empty());
    }

    /// `register_consensus_timeout` records `now + delay`;
    /// `unregister_consensus_timeout` drops it. `apply_consensus_outcome`
    /// uses the unregister path to drop deadlines on natural resolution
    /// so `tick_deadlines` doesn't fire a stale `handle_consensus_timeout`.
    #[test]
    fn register_then_unregister_consensus_timeout() {
        let mut conversation = make_conversation_working();
        let before = Instant::now();
        conversation.register_consensus_timeout(42, Duration::from_secs(30));
        let fire_at = conversation.pending_consensus_timeouts[&42];
        assert!(fire_at > before + Duration::from_secs(29));
        assert!(fire_at < Instant::now() + Duration::from_secs(31));

        conversation.unregister_consensus_timeout(42);
        assert!(!conversation.pending_consensus_timeouts.contains_key(&42));

        // Unregistering an unknown id is a no-op (no panic, no error).
        conversation.unregister_consensus_timeout(999);
    }

    // ── create_commit_candidate guards ──────────────────────────────────

    #[test]
    fn create_commit_candidate_errors_for_non_steward_outside_recovery() {
        let mut conversation = make_conversation_with_steward(StubStewardList::member());
        let err = conversation
            .create_commit_candidate(&UnusedSigner, b"me")
            .expect_err("non-steward should be rejected");
        assert!(matches!(err, ConversationError::NotASteward));
    }

    #[test]
    fn create_commit_candidate_errors_when_no_approved_proposals() {
        let mut conversation = make_conversation_with_steward(StubStewardList::steward());
        let err = conversation
            .create_commit_candidate(&UnusedSigner, b"me")
            .expect_err("empty approved queue should be rejected");
        assert!(matches!(err, ConversationError::NoProposals));
    }

    /// An emergency-criteria proposal in the approved queue must surface as
    /// `UnexpectedNonMlsProposals` — only MLS-producing payloads belong in a
    /// commit. The error carries the offending proposal ids so the
    /// orchestrator can drop them.
    #[test]
    fn create_commit_candidate_errors_on_emergency_in_approved_queue() {
        use crate::protos::de_mls::messages::v1::ViolationEvidence;

        let mut conversation = make_conversation_with_steward(StubStewardList::steward());
        let emergency = ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new())
            .with_creator(vec![0x01])
            .into_update_request()
            .unwrap();
        conversation.queues.insert_approved_proposal(50, emergency);

        let err = conversation
            .create_commit_candidate(&UnusedSigner, b"me")
            .expect_err("emergency in approved queue should be rejected");
        let ConversationError::UnexpectedNonMlsProposals { proposal_ids } = err else {
            panic!("expected UnexpectedNonMlsProposals, got {err:?}");
        };
        assert_eq!(proposal_ids, vec![50]);
    }
}
