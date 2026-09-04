//! The [`Engine`] struct and the coordinators every other engine module
//! builds on: the per-call [`Output`] accumulator, the state machine and
//! phase timer, deadline registration, scoring, and the facts the router
//! hands in (own id, epoch, member set).
//!
//! Every driving call sets `self.now`, does its work through the helpers
//! here, and returns `finish()`. Sibling modules extend `Engine` with
//! further `impl` blocks.

use std::{collections::HashMap, time::Duration};

use hashgraph_like_consensus::{
    events::ConsensusEventBus, scope::ScopeID, service::ConsensusService,
    storage::InMemoryConsensusStorage,
};
use tracing::{info, warn};

use crate::{
    ConversationError, ScoreChange, ScoreOp, ScoreSnapshot, StewardListService,
    engine::{
        commit::CommitRoundBuffer,
        config::EngineConfig,
        consensus_signer::MemberSigner,
        outcome_bus::{OutcomeBus, OutcomeReceiver},
        phase_timer::PhaseTimer,
        queues::EngineQueues,
        state_machine::ConversationStateMachine,
        store::EngineStore,
        types::{
            Action, CommitHash, Decision, Event, MemberId, Outbound, Output, Phase, StagedFacts,
            Timestamp,
        },
    },
    peer_scoring::PeerScoringService,
};

/// The consensus service the engine runs: in-memory sessions, the engine's
/// outcome bus, and the identity-only signer.
pub(crate) type EngineConsensus =
    ConsensusService<ScopeID, InMemoryConsensusStorage<ScopeID>, OutcomeBus, MemberSigner>;

/// One pending auto-vote: cast `vote` for `proposal_id` once `fire_at`
/// passes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AutoVoteEntry {
    pub(crate) fire_at: Timestamp,
    pub(crate) vote: bool,
}

/// Time-driven state walked on every `tick`.
pub(crate) struct Timing {
    /// Anchor for the active phase: freeze start, or the first approved
    /// proposal while `Working`.
    pub(crate) phase_timer: PhaseTimer,
    /// Pending auto-votes by proposal id.
    pub(crate) pending_auto_votes: HashMap<u32, AutoVoteEntry>,
    /// Pending consensus-session timeouts: proposal id to fire time.
    pub(crate) pending_consensus_timeouts: HashMap<u32, Timestamp>,
    /// Last `CommitRoundProgress` emitted; reset outside `Freezing`.
    pub(crate) last_commit_round_progress: Option<(usize, usize)>,
    /// Backup-steward proposal takeover anchor.
    pub(crate) buffered_propose_anchor: Option<Timestamp>,
    /// Backup-steward sync re-send anchor.
    pub(crate) sync_resend_anchor: Option<Timestamp>,
    /// Pull-side sync request anchor.
    pub(crate) sync_request_anchor: Option<Timestamp>,
    /// Consecutive unanswered sync requests.
    pub(crate) unanswered_sync_rounds: u32,
}

impl Timing {
    fn new() -> Self {
        Self {
            phase_timer: PhaseTimer::new(),
            pending_auto_votes: HashMap::new(),
            pending_consensus_timeouts: HashMap::new(),
            last_commit_round_progress: None,
            buffered_propose_anchor: None,
            sync_resend_anchor: None,
            sync_request_anchor: None,
            unanswered_sync_rounds: 0,
        }
    }
}

/// A `BuildCommit` decision the router has not reported back on yet: the
/// batch it was asked to commit.
#[derive(Debug, Clone)]
pub(crate) struct PendingBuild {
    pub(crate) actions: Vec<Action>,
}

/// A `Merge` decision the router has not reported back on yet.
#[derive(Debug, Clone)]
pub(crate) struct PendingMerge {
    pub(crate) hash: CommitHash,
    /// Facts of the winning candidate, or `None` for our own commit.
    pub(crate) facts: Option<StagedFacts>,
    pub(crate) is_local: bool,
}

/// One conversation's protocol state, with no group, provider, signer or
/// clock inside. The router drives it and executes what it returns.
pub struct Engine<St: EngineStore> {
    pub(crate) conversation_id: String,
    /// This member's id (its signature key).
    pub(crate) own: Vec<u8>,
    /// The group's epoch as last reported by the router.
    pub(crate) epoch: u64,
    /// The group's member set as last reported by the router, sorted.
    pub(crate) members: Vec<Vec<u8>>,
    pub(crate) queues: EngineQueues,
    pub(crate) state_machine: ConversationStateMachine,
    pub(crate) steward_list: StewardListService,
    pub(crate) scoring: PeerScoringService,
    pub(crate) consensus: EngineConsensus,
    pub(crate) consensus_rx: OutcomeReceiver,
    pub(crate) config: EngineConfig,
    pub(crate) timing: Timing,
    pub(crate) store: St,
    /// The current driving call's clock reading.
    pub(crate) now: Timestamp,
    /// What the current driving call has produced so far.
    pub(crate) out: Output,
    pub(crate) pending_build: Option<PendingBuild>,
    pub(crate) pending_merge: Option<PendingMerge>,
    /// The candidates collected for the epoch's commit round. Ephemeral: a
    /// restart rejoins the next round rather than resuming this one.
    pub(crate) round: CommitRoundBuffer,
}

impl<St: EngineStore> Engine<St> {
    /// Assemble an engine over the facts the router supplied. Installs no
    /// steward list; the constructors do that.
    pub(crate) fn assemble(
        conversation_id: &str,
        own: MemberId,
        epoch: u64,
        members: &[MemberId],
        config: EngineConfig,
        store: St,
    ) -> Result<Self, ConversationError> {
        config.validate()?;
        let scoring =
            PeerScoringService::new(crate::default_score_deltas(), config.scoring.clone());
        scoring.validate_config()?;

        let mut steward_list = StewardListService::empty(config.steward_list.clone());
        steward_list.set_conversation_id(conversation_id.as_bytes());

        let outcome_bus = OutcomeBus::default();
        let consensus_rx = outcome_bus.subscribe();
        let consensus = ConsensusService::new_with_components(
            InMemoryConsensusStorage::new(),
            outcome_bus,
            MemberSigner::new(own.as_bytes().to_vec()),
            config.max_consensus_sessions,
        );

        let mut sorted: Vec<Vec<u8>> = members.iter().map(|m| m.as_bytes().to_vec()).collect();
        sorted.sort();
        sorted.dedup();

        Ok(Self {
            conversation_id: conversation_id.to_string(),
            own: own.as_bytes().to_vec(),
            epoch,
            members: sorted,
            queues: EngineQueues::new(config.dedup_window),
            state_machine: ConversationStateMachine::new_as_member(),
            steward_list,
            scoring,
            consensus,
            consensus_rx,
            config,
            timing: Timing::new(),
            store,
            now: Timestamp::ZERO,
            out: Output::default(),
            pending_build: None,
            pending_merge: None,
            round: CommitRoundBuffer::default(),
        })
    }

    // ── queries ────────────────────────────────────────────────────────

    /// The conversation id, which is also the MLS group id.
    pub fn id(&self) -> &str {
        &self.conversation_id
    }

    /// This member.
    pub fn own_id(&self) -> MemberId {
        MemberId::from(self.own.as_slice())
    }

    /// The epoch the router last reported.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The member set the router last reported.
    pub fn members(&self) -> Vec<MemberId> {
        self.members
            .iter()
            .map(|m| MemberId::from(m.as_slice()))
            .collect()
    }

    /// The lifecycle phase.
    pub fn phase(&self) -> Phase {
        self.state_machine.current_state()
    }

    /// Whether this member is on the current steward list.
    pub fn is_steward(&self) -> bool {
        self.steward_list.is_steward(&self.own)
    }

    /// Whether this member is the epoch steward at the current epoch.
    pub fn is_epoch_steward(&self) -> bool {
        let eligible = self.queues.steward_eligibility(&self.members);
        self.steward_list
            .epoch_steward(self.epoch, &eligible)
            .is_some_and(|s| s == self.own.as_slice())
    }

    /// Whether the steward list is installed and covers the current epoch.
    pub fn is_synced(&self) -> bool {
        self.steward_list
            .current_list()
            .is_some_and(|_| !self.steward_list.is_exhausted(self.epoch))
    }

    /// The durable store, for the router to flush.
    pub fn store(&self) -> &St {
        &self.store
    }

    /// The timing and policy this conversation runs with.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    pub(crate) fn current_state(&self) -> Phase {
        self.state_machine.current_state()
    }

    /// Whether `member` is in the group as last reported.
    pub(crate) fn is_member(&self, member: &[u8]) -> bool {
        self.members
            .binary_search_by(|m| m.as_slice().cmp(member))
            .is_ok()
    }

    // ── per-call output ────────────────────────────────────────────────

    /// Start a driving call at `now`.
    pub(crate) fn begin(&mut self, now: Timestamp) {
        self.now = now;
    }

    /// End a driving call: drain pending consensus outcomes, compute the
    /// next wakeup, and hand back everything the call produced.
    pub(crate) fn finish(&mut self) -> Output {
        self.drain_consensus_outcomes();
        self.anchor_inactivity_timer();
        let mut out = std::mem::take(&mut self.out);
        out.wakeup = self.next_wakeup_in();
        out
    }

    /// Approved work waiting in `Working` starts the commit-inactivity
    /// clock the moment it lands, so the wakeup this call reports already
    /// covers the round that has to follow.
    fn anchor_inactivity_timer(&mut self) {
        if self.current_state() == Phase::Working
            && self.queues.approved_proposals_count() > 0
            && self.timing.phase_timer.started_at().is_none()
        {
            self.timing.phase_timer.start(self.now);
        }
    }

    /// Record an event for the router.
    pub(crate) fn emit(&mut self, event: Event) {
        self.out.events.push(event);
    }

    /// Record a phase change, if there was one.
    pub(crate) fn emit_phase(&mut self, state: Option<Phase>) {
        if state.is_some() {
            let phase = self.phase();
            self.emit(Event::PhaseChange(phase));
        }
    }

    /// Queue a control message for the router to seal and broadcast.
    pub(crate) fn send_control(&mut self, bytes: Vec<u8>) {
        self.out.outbound.push(Outbound::Control(bytes));
    }

    /// Queue a candidate envelope for the router to broadcast in the clear.
    pub(crate) fn send_candidate(&mut self, envelope: Vec<u8>) {
        self.out.outbound.push(Outbound::Candidate(envelope));
    }

    /// Ask the router to do something against the group.
    pub(crate) fn decide(&mut self, decision: Decision) {
        self.out.decisions.push(decision);
    }

    /// Report a failure on a path the router never called.
    pub(crate) fn report_failure(&mut self, operation: &str, error: &ConversationError) {
        warn!(conversation = %self.conversation_id, operation, error = %error, "engine step failed");
        self.emit(Event::Error {
            operation: operation.to_owned(),
            message: error.to_string(),
        });
    }

    /// A proposal the engine raised for itself can be turned away while the
    /// group is mid-rotation; the buffer retries it. Anything else failed.
    pub(crate) fn report_deferred_proposal(&mut self, operation: &str, error: ConversationError) {
        if matches!(
            error,
            ConversationError::ConversationBlocked(_) | ConversationError::PartialFreeze
        ) {
            info!(conversation = %self.conversation_id, error = %error, "proposal deferred");
            return;
        }
        self.report_failure(operation, &error);
    }

    // ── scoring ────────────────────────────────────────────────────────

    /// Apply `ops` to the score table, reporting each moved score.
    pub(crate) fn apply_score_ops(&mut self, ops: &[ScoreOp]) {
        let changes = self.scoring.apply_ops(ops);
        self.emit_score_changes(changes);
    }

    /// Adopt a bootstrap score snapshot, reporting each moved score.
    pub(crate) fn apply_score_snapshot(&mut self, snapshot: &ScoreSnapshot) {
        let changes = self.scoring.apply_snapshot(snapshot);
        self.emit_score_changes(changes);
    }

    fn emit_score_changes(&mut self, changes: Vec<ScoreChange>) {
        for change in changes {
            self.emit(Event::MemberScoreChanged {
                member: MemberId::from(change.member_id.as_slice()),
                previous: change.previous,
                score: change.score,
            });
        }
    }

    // ── deadlines ──────────────────────────────────────────────────────

    /// Earliest pending deadline relative to `now`, or `None`.
    fn next_wakeup_in(&self) -> Option<Duration> {
        let earliest = self
            .timing
            .pending_consensus_timeouts
            .values()
            .copied()
            .chain(self.timing.pending_auto_votes.values().map(|e| e.fire_at))
            .chain(self.phase_deadline())
            .chain(self.sync_request_deadline())
            .chain(self.takeover_deadlines())
            .min()?;
        Some(earliest.saturating_duration_since(self.now))
    }

    fn sync_request_deadline(&self) -> Option<Timestamp> {
        self.timing
            .sync_request_anchor
            .map(|anchor| anchor + self.config.backup_takeover_window)
    }

    /// The backup-takeover anchors, so a backup's turn wakes the router.
    fn takeover_deadlines(&self) -> impl Iterator<Item = Timestamp> {
        let window = self.config.backup_takeover_window;
        [
            self.timing.buffered_propose_anchor.map(|a| a + window),
            self.timing.sync_resend_anchor.map(|a| a + window),
        ]
        .into_iter()
        .flatten()
    }

    fn phase_deadline(&self) -> Option<Timestamp> {
        let anchor = self.timing.phase_timer.started_at()?;
        match self.current_state() {
            Phase::Freezing => Some(anchor + self.config.freeze_duration),
            Phase::Working if self.queues.approved_proposals_count() > 0 => {
                Some(anchor + self.config.commit_batch_window)
            }
            _ => None,
        }
    }

    /// Register an auto-vote `delay` from now. Re-registering replaces.
    pub(crate) fn register_auto_vote(&mut self, proposal_id: u32, delay: Duration, vote: bool) {
        self.timing.pending_auto_votes.insert(
            proposal_id,
            AutoVoteEntry {
                fire_at: self.now + delay,
                vote,
            },
        );
    }

    pub(crate) fn cancel_auto_vote(&mut self, proposal_id: u32) {
        self.timing.pending_auto_votes.remove(&proposal_id);
    }

    pub(crate) fn cancel_all_auto_votes(&mut self) {
        self.timing.pending_auto_votes.clear();
    }

    /// Register a consensus-session timeout `delay` from now.
    pub(crate) fn register_consensus_timeout(&mut self, proposal_id: u32, delay: Duration) {
        self.timing
            .pending_consensus_timeouts
            .insert(proposal_id, self.now + delay);
    }

    pub(crate) fn unregister_consensus_timeout(&mut self, proposal_id: u32) {
        self.timing.pending_consensus_timeouts.remove(&proposal_id);
    }

    // ── state machine + phase timer ────────────────────────────────────

    pub(crate) fn start_working(&mut self) -> Phase {
        self.state_machine.start_working();
        self.timing.phase_timer.clear();
        info!(state = "Working", "state transition");
        Phase::Working
    }

    /// Enter `Freezing` from `Working`, anchoring the
    /// freeze timer. `None` from any other state.
    pub(crate) fn start_freezing(&mut self) -> Option<Phase> {
        if self.state_machine.start_freezing() {
            self.timing.phase_timer.start(self.now);
            info!(state = "Freezing", "state transition");
            Some(Phase::Freezing)
        } else {
            None
        }
    }

    pub(crate) fn start_selection(&mut self) -> Phase {
        self.state_machine.start_selection();
        info!(state = "Selection", "state transition");
        Phase::Selection
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub(crate) fn is_freeze_window_elapsed(&self) -> bool {
        self.current_state() == Phase::Freezing
            && self
                .timing
                .phase_timer
                .elapsed_since_anchor(self.now, self.config.freeze_duration)
    }

    /// The "steward waited too long" transition into `Freezing`. `Some`
    /// exactly on the call that transitions. Anchors itself on the first
    /// call with approved work.
    pub(crate) fn check_steward_inactivity(
        &mut self,
        approved_proposals_count: usize,
        inactivity_duration: Duration,
    ) -> Option<Phase> {
        if self.current_state() != Phase::Working || approved_proposals_count == 0 {
            return None;
        }
        if self.timing.phase_timer.started_at().is_none() {
            self.timing.phase_timer.start(self.now);
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
            .elapsed_since_anchor(self.now, inactivity_duration)
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
