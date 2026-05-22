//! [`SessionRunner`] struct, constructor, and the state-machine + phase-timer
//! coordinators that compose [`crate::core::ConversationHandle`] with
//! [`crate::app::PhaseTimer`] under one lock. Per-conversation method
//! bodies (proposal submission, voting, inbound dispatch, etc.) live in
//! sibling modules and extend `SessionRunner` via additional `impl` blocks.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tracing::info;

use crate::{
    app::{PhaseTimer, SessionTick, UserError},
    core::{
        ConsensusPlugin, Conversation, ConversationConfig, ConversationHandle,
        ConversationPluginsFactory, ConversationState, ConversationStateMachine, PluginConsensus,
        SessionEvent,
    },
    ds::{OutboundPacket, SharedDeliveryService},
};

/// Free helper that publishes a packet on the supplied transport. Pure sync —
/// the caller's task does the publish directly. Multi-thread integrators
/// that want the publish off-runtime can wrap the call site in
/// `spawn_blocking` themselves.
pub(crate) fn send_packet(
    transport: &SharedDeliveryService,
    packet: OutboundPacket,
) -> Result<(), UserError> {
    transport
        .lock()
        .map_err(|_| UserError::LockPoisoned("transport"))?
        .publish(packet)?;
    Ok(())
}

/// Default capacity for a session's [`SessionEvent`] broadcast channel.
/// Sized for bursty proposal sessions (proposals + votes + UI pushes in
/// One pending auto-vote: cast `vote` for `proposal_id` once the wall-clock
/// catches up to `fire_at`. Registered by `initiate_proposal` (Deferred
/// path) and `on_incoming_proposal`; cancelled on manual vote or consensus
/// resolution; fired by [`SessionRunner::tick_deadlines`].
#[derive(Debug, Clone, Copy)]
pub struct AutoVoteEntry {
    pub fire_at: Instant,
    pub vote: bool,
}

pub struct SessionRunner<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Conversation name. Identifies this session in the User registry and
    /// is used to construct scope keys for consensus operations.
    pub conversation_id: String,
    pub(crate) handle: ConversationHandle<CP>,
    /// Per-conversation consensus service. Owns this conversation's scope
    /// in the shared storage and a private event bus. Constructed at
    /// conversation creation by `User::build_consensus_service` and held
    /// here so consensus calls hit the local service directly without
    /// User-level lookup. `pub` so integrators can reach
    /// `session.consensus.event_bus().subscribe()` for per-conv consensus
    /// event forwarding.
    pub consensus: PluginConsensus<P>,
    /// Wall-clock anchor combined with `handle.state_machine` by
    /// coordinator methods.
    phase_timer: PhaseTimer,
    /// Pending auto-votes by `proposal_id`. Walked by
    /// [`Self::tick_deadlines`]; each entry whose `fire_at` has passed
    /// gets a `cast_vote` and is removed from the map. Cancelled (removed)
    /// when a manual vote arrives or the consensus session resolves.
    pub pending_auto_votes: HashMap<u32, AutoVoteEntry>,
    /// Pending consensus-session timeouts: `proposal_id -> fire_at`.
    /// Registered when a proposal opens (own or incoming peer); fired by
    /// [`Self::tick_deadlines`] which calls
    /// `consensus.handle_consensus_timeout`. Removed when the session
    /// resolves naturally via `apply_consensus_outcome`.
    pub pending_consensus_timeouts: HashMap<u32, Instant>,
    /// Synchronous outbound transport (cloned from `User`). Per-session
    /// methods reach this via [`Self::transport`] and route through
    /// [`send_packet`], a direct sync publish.
    transport: SharedDeliveryService,
    /// Identity bytes derived from `User.member_id.member_id_bytes()` at
    /// session construction. Stored as `Arc<[u8]>` so hot-path session
    /// code can clone the handle cheaply across lock-guard drops.
    pub self_member_id: Arc<[u8]>,
    /// Display form derived from `User.member_id.member_id_display()` at
    /// session construction. `Arc<str>` for the same reason as
    /// `self_member_id` — cheap clone across guard boundaries.
    pub member_id_display: Arc<str>,
    /// Per-User instance UUID (cloned from `User`). Tagged on every
    /// outbound packet for self-message filtering.
    pub app_id: Arc<[u8]>,
    /// Pending [`SessionEvent`]s waiting for a caller to drain. Interior
    /// `Mutex` so producer-side `emit_event` stays `&self`; consumers
    /// drain via [`Self::drain_events`] once per polling cycle.
    pending_events: Mutex<Vec<SessionEvent>>,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Build a fresh runner. Creator path passes `Some(mls)`; joiner
    /// path passes `None` and attaches the MLS service later via
    /// `handle.attach_mls`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        conversation_id: String,
        conversation: Conversation,
        mls: Option<CP::Mls>,
        state_machine: ConversationStateMachine,
        phase_timer: PhaseTimer,
        config: ConversationConfig,
        scoring: CP::Scoring,
        steward_list: CP::StewardList,
        consensus: PluginConsensus<P>,
        transport: SharedDeliveryService,
        self_member_id: Arc<[u8]>,
        member_id_display: Arc<str>,
        app_id: Arc<[u8]>,
    ) -> Self {
        Self {
            conversation_id,
            handle: ConversationHandle::new(
                conversation,
                mls,
                state_machine,
                config,
                scoring,
                steward_list,
            ),
            consensus,
            phase_timer,
            pending_auto_votes: HashMap::new(),
            pending_consensus_timeouts: HashMap::new(),
            transport,
            self_member_id,
            member_id_display,
            app_id,
            pending_events: Mutex::new(Vec::new()),
        }
    }

    /// Append a [`SessionEvent`] to the pending-events buffer. The caller's
    /// polling cycle drains it via [`Self::drain_events`]. Stays `&self`
    /// thanks to the interior [`Mutex`], so the many session-coordinator
    /// methods that emit during a brief read guard don't need to escalate
    /// to a write guard. Silent on poison — emit is fire-and-forget.
    pub(crate) fn emit_event(&self, event: SessionEvent) {
        if let Ok(mut buf) = self.pending_events.lock() {
            buf.push(event);
        }
    }

    /// Drain every pending [`SessionEvent`] accumulated since the last
    /// call. Returns events in insertion order. Callers (UI fanout,
    /// audit log) invoke this once per polling cycle.
    pub fn drain_events(&self) -> Vec<SessionEvent> {
        match self.pending_events.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
        }
    }

    /// Smallest pending deadline relative to now, or `None` when nothing
    /// is scheduled. Returns `Some(Duration::ZERO)` for an already-elapsed
    /// deadline. Covers consensus-session timeouts, auto-vote timers, and
    /// state-machine phase deadlines (Freezing window, PendingJoin
    /// expiry, steward / recovery inactivity). Forward to an external
    /// scheduler that calls [`crate::app::User::poll_session`] on fire;
    /// extra/early wakeups are no-ops.
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
    /// session's polling paths (`poll_freeze_status`, `check_member_freeze`,
    /// `check_pending_join`, and the inactivity check in
    /// `check_steward_inactivity`) all gate on the phase timer; this
    /// surfaces the same wall-clock target so an external scheduler can
    /// wake us at the right time.
    fn phase_deadline(&self) -> Option<Instant> {
        let anchor = self.phase_timer.started_at()?;
        let cfg = &self.handle.config;
        match self.handle.current_state() {
            ConversationState::Freezing => Some(anchor + cfg.freeze_duration),
            ConversationState::PendingJoin => Some(anchor + cfg.commit_inactivity_duration * 3),
            ConversationState::Working => {
                if self.handle.conversation.approved_proposals_count() == 0 {
                    return None;
                }
                let dur = if self.handle.is_in_recovery_mode() {
                    cfg.recovery_inactivity_duration
                } else {
                    cfg.commit_inactivity_duration
                };
                Some(anchor + dur)
            }
            _ => None,
        }
    }

    /// Snapshot the earliest deadline into a [`SessionTick`]. Public ops
    /// returning `SessionTick` call this at the end of their happy path
    /// so the caller gets a wakeup hint without a second accessor call.
    pub(crate) fn tick(&self) -> SessionTick {
        SessionTick {
            next_wakeup_in: self.next_wakeup_in(),
        }
    }

    /// Borrow the session's transport without taking the runner lock.
    /// Cheap to clone the inner `Arc` and publish after dropping the
    /// runner guard.
    pub fn transport(&self) -> &SharedDeliveryService {
        &self.transport
    }

    // ── Pending deadlines (auto-votes + consensus timeouts) ─────────

    /// Register an auto-vote to fire `delay` from now with the given
    /// `vote` choice. Idempotent — re-registering for the same
    /// `proposal_id` replaces the existing entry.
    pub fn register_auto_vote(&mut self, proposal_id: u32, delay: Duration, vote: bool) {
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
    pub fn cancel_auto_vote(&mut self, proposal_id: u32) {
        self.pending_auto_votes.remove(&proposal_id);
    }

    /// Drop every pending auto-vote on this runner. Called on conversation
    /// leave so no stale entries fire against a conversation we've left.
    pub fn cancel_all_auto_votes(&mut self) {
        self.pending_auto_votes.clear();
    }

    /// Register a consensus-session timeout. Fires `delay` from now via
    /// [`Self::tick_deadlines`]; removed naturally on consensus resolution.
    pub fn register_consensus_timeout(&mut self, proposal_id: u32, delay: Duration) {
        self.pending_consensus_timeouts
            .insert(proposal_id, Instant::now() + delay);
    }

    /// Drop the pending consensus timeout for `proposal_id`. Called from
    /// `apply_consensus_outcome` once the library reaches/fails consensus,
    /// so the timeout can't fire a stale `handle_consensus_timeout` against
    /// an already-resolved session.
    pub fn unregister_consensus_timeout(&mut self, proposal_id: u32) {
        self.pending_consensus_timeouts.remove(&proposal_id);
    }

    // ── State-machine + phase-timer coordinators ────────────────────

    pub(crate) fn start_working(&mut self) -> ConversationState {
        self.handle.state_machine.start_working();
        self.phase_timer.clear();
        info!(state = "Working", "state transition");
        ConversationState::Working
    }

    pub(crate) fn start_freezing(&mut self) -> ConversationState {
        self.handle.state_machine.start_freezing();
        self.phase_timer.start();
        info!(state = "Freezing", "state transition");
        ConversationState::Freezing
    }

    /// Bypass the inactivity timer and enter Freezing immediately. Returns
    /// `Some(Freezing)` on transition (only fires from Working or
    /// Reelection); `None` from other states.
    pub(crate) fn force_freezing(&mut self) -> Option<ConversationState> {
        if self.handle.state_machine.force_freezing() {
            self.phase_timer.start();
            info!(state = "Freezing", "state transition (forced)");
            Some(ConversationState::Freezing)
        } else {
            None
        }
    }

    pub(crate) fn start_selection(&mut self) -> ConversationState {
        self.handle.state_machine.start_selection();
        info!(state = "Selection", "state transition");
        ConversationState::Selection
    }

    pub(crate) fn start_reelection(&mut self) -> ConversationState {
        self.handle.state_machine.start_reelection();
        self.phase_timer.clear();
        info!(state = "Reelection", "state transition");
        ConversationState::Reelection
    }

    /// `true` once 3× `commit_inactivity_duration` has passed in
    /// `PendingJoin` without a welcome.
    pub fn is_pending_join_expired(&self) -> bool {
        self.handle.current_state() == ConversationState::PendingJoin
            && self
                .phase_timer
                .elapsed_since_anchor(self.handle.config.commit_inactivity_duration * 3)
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub fn is_freeze_timed_out(&self) -> bool {
        self.handle.current_state() == ConversationState::Freezing
            && self
                .phase_timer
                .elapsed_since_anchor(self.handle.config.freeze_duration)
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
        if self.handle.current_state() != ConversationState::Working
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
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;
    use crate::core::Conversation;
    use crate::defaults::DefaultConsensusPlugin;
    use crate::test_fixtures::{
        StubPluginsFactory, StubScoring, StubStewardList, UnusedMls, make_test_consensus_service,
    };

    fn make_runner_pending_join(
        commit_inactivity: Duration,
    ) -> SessionRunner<DefaultConsensusPlugin, StubPluginsFactory> {
        let config = ConversationConfig {
            commit_inactivity_duration: commit_inactivity,
            ..ConversationConfig::default()
        };
        let mut runner = SessionRunner::new(
            "g".to_string(),
            Conversation::new("g"),
            Some(UnusedMls),
            ConversationStateMachine::new_as_pending_join(),
            PhaseTimer::new(),
            config,
            StubScoring,
            StubStewardList::member(),
            make_test_consensus_service(),
            Arc::new(Mutex::new(crate::test_fixtures::UnusedTransport)),
            Arc::from(&b"test-member-id"[..]),
            Arc::from("0xtest-display"),
            Arc::from(&[0u8; 16][..]),
        );
        runner.phase_timer.start();
        runner
    }

    fn make_runner_working() -> SessionRunner<DefaultConsensusPlugin, StubPluginsFactory> {
        SessionRunner::new(
            "g".to_string(),
            Conversation::new("g"),
            Some(UnusedMls),
            ConversationStateMachine::new_as_member(),
            PhaseTimer::new(),
            ConversationConfig::default(),
            StubScoring,
            StubStewardList::member(),
            make_test_consensus_service(),
            Arc::new(Mutex::new(crate::test_fixtures::UnusedTransport)),
            Arc::from(&b"test-member-id"[..]),
            Arc::from("0xtest-display"),
            Arc::from(&[0u8; 16][..]),
        )
    }

    /// `is_pending_join_expired` flips once 3× `commit_inactivity_duration`
    /// has passed since the anchor. Test backdates the anchor to avoid
    /// a real wall-clock wait.
    #[test]
    fn pending_join_expires_after_three_times_commit_inactivity() {
        let inactivity = Duration::from_millis(50);
        let mut runner = make_runner_pending_join(inactivity);

        assert!(
            !runner.is_pending_join_expired(),
            "fresh anchor must not be expired"
        );

        // Just inside the window: anchor 2.5× inactivity in the past.
        runner
            .phase_timer
            .set_started_at_for_test(Some(Instant::now() - inactivity * 5 / 2));
        assert!(
            !runner.is_pending_join_expired(),
            "before 3× boundary must not be expired"
        );

        // Past the boundary: anchor 4× inactivity in the past.
        runner
            .phase_timer
            .set_started_at_for_test(Some(Instant::now() - inactivity * 4));
        assert!(
            runner.is_pending_join_expired(),
            "past 3× boundary must be expired"
        );
    }

    /// Outside `PendingJoin`, `is_pending_join_expired` always returns false
    /// regardless of how old the anchor is.
    #[test]
    fn pending_join_expired_only_in_pending_join_state() {
        let mut runner = make_runner_working();
        runner
            .phase_timer
            .set_started_at_for_test(Some(Instant::now() - Duration::from_secs(3600)));
        assert!(
            !runner.is_pending_join_expired(),
            "Working state must never report pending-join-expired"
        );
    }

    /// First tick with approved work auto-anchors the timer and returns `None`.
    /// Second tick before timeout still returns `None`. State must remain Working.
    #[test]
    fn check_steward_inactivity_first_tick_anchors_and_returns_none() {
        let mut runner = make_runner_working();
        assert_eq!(runner.handle.current_state(), ConversationState::Working);
        assert!(
            runner.phase_timer.started_at().is_none(),
            "fresh runner has no anchor"
        );

        let result =
            runner.check_steward_inactivity(/* approved */ 1, Duration::from_secs(10));

        assert_eq!(result, None, "first tick auto-anchors and returns None");
        assert!(
            runner.phase_timer.started_at().is_some(),
            "anchor must be set after first tick"
        );
        assert_eq!(
            runner.handle.current_state(),
            ConversationState::Working,
            "state must stay Working until inactivity actually elapses"
        );

        let result =
            runner.check_steward_inactivity(/* approved */ 1, Duration::from_secs(10));
        assert_eq!(
            result, None,
            "second tick before timeout still returns None"
        );
    }

    /// No approved work → no anchor started, no transition.
    #[test]
    fn check_steward_inactivity_noop_without_approved_work() {
        let mut runner = make_runner_working();
        let result = runner.check_steward_inactivity(0, Duration::from_secs(10));
        assert_eq!(result, None);
        assert!(
            runner.phase_timer.started_at().is_none(),
            "no approved work must not start the timer"
        );
    }

    // ── Caller-polled deadlines + drain model ───────────────────────────

    /// `emit_event` appends and `drain_events` returns insertion-ordered.
    /// Establishes the contract relied on by integration tests that build
    /// up an event log over multiple polling cycles.
    #[test]
    fn emit_event_then_drain_returns_insertion_order_and_clears_buffer() {
        let runner = make_runner_working();
        runner.emit_event(SessionEvent::PhaseChange(ConversationState::Working));
        runner.emit_event(SessionEvent::Leaving);

        let drained = runner.drain_events();
        assert_eq!(drained.len(), 2);
        assert!(matches!(
            drained[0],
            SessionEvent::PhaseChange(ConversationState::Working)
        ));
        assert!(matches!(drained[1], SessionEvent::Leaving));

        // Second drain returns empty — buffer was cleared.
        assert!(runner.drain_events().is_empty());
    }

    /// `register_auto_vote` is idempotent — re-registering the same
    /// `proposal_id` replaces the previous entry rather than stacking.
    /// Caller relies on this when re-anchoring an auto-vote on a `Deferred`
    /// re-submit.
    #[test]
    fn register_auto_vote_replaces_existing_entry() {
        let mut runner = make_runner_working();
        runner.register_auto_vote(7, Duration::from_secs(10), true);
        let first_fire = runner.pending_auto_votes[&7].fire_at;

        // Re-register with a different `vote` and a longer delay; the
        // second insert must overwrite, not co-exist.
        std::thread::sleep(Duration::from_millis(2));
        runner.register_auto_vote(7, Duration::from_secs(20), false);
        assert_eq!(runner.pending_auto_votes.len(), 1);
        let entry = runner.pending_auto_votes[&7];
        assert!(!entry.vote);
        assert!(entry.fire_at > first_fire);
    }

    /// `cancel_auto_vote` drops one entry. `cancel_all_auto_votes` drops
    /// every entry. Both are the only paths that should remove pending
    /// auto-votes from outside `tick_deadlines`.
    #[test]
    fn cancel_auto_vote_removes_only_the_targeted_proposal() {
        let mut runner = make_runner_working();
        runner.register_auto_vote(1, Duration::from_secs(5), true);
        runner.register_auto_vote(2, Duration::from_secs(5), false);
        runner.register_auto_vote(3, Duration::from_secs(5), true);

        runner.cancel_auto_vote(2);
        assert!(runner.pending_auto_votes.contains_key(&1));
        assert!(!runner.pending_auto_votes.contains_key(&2));
        assert!(runner.pending_auto_votes.contains_key(&3));

        runner.cancel_all_auto_votes();
        assert!(runner.pending_auto_votes.is_empty());
    }

    /// `register_consensus_timeout` records `now + delay`;
    /// `unregister_consensus_timeout` drops it. `apply_consensus_outcome`
    /// uses the unregister path to drop deadlines on natural resolution
    /// so `tick_deadlines` doesn't fire a stale `handle_consensus_timeout`.
    #[test]
    fn register_then_unregister_consensus_timeout() {
        let mut runner = make_runner_working();
        let before = Instant::now();
        runner.register_consensus_timeout(42, Duration::from_secs(30));
        let fire_at = runner.pending_consensus_timeouts[&42];
        assert!(fire_at > before + Duration::from_secs(29));
        assert!(fire_at < Instant::now() + Duration::from_secs(31));

        runner.unregister_consensus_timeout(42);
        assert!(!runner.pending_consensus_timeouts.contains_key(&42));

        // Unregistering an unknown id is a no-op (no panic, no error).
        runner.unregister_consensus_timeout(999);
    }
}
