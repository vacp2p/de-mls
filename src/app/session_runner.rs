//! App-side per-conversation runner: wraps a [`crate::core::ConversationHandle`]
//! together with a [`crate::app::PhaseTimer`] and the per-proposal
//! auto-vote timer registry. Coordinator methods compose state-machine
//! transitions with phase-timer anchors so callers update both in one call.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{sync::broadcast, task::JoinHandle};
use tracing::info;

use crate::{
    app::PhaseTimer,
    core::{
        ConsensusPlugin, Conversation, ConversationConfig, ConversationHandle,
        ConversationPluginsFactory, ConversationState, ConversationStateMachine, PluginConsensus,
        SessionEvent,
    },
};

/// Default capacity for a session's [`SessionEvent`] broadcast channel.
/// Sized for bursty proposal sessions (proposals + votes + UI pushes in
/// flight); subscribers that fall behind by more than this lose events.
const SESSION_EVENT_CAPACITY: usize = 256;

/// Per-conversation auto-vote timer registry. Spawned when a proposal first
/// becomes visible locally (own submit or peer inbound); cancelled on
/// manual vote, consensus resolution, or conversation leave.
pub(crate) type AutoVoteTimers = Arc<Mutex<HashMap<u32, JoinHandle<()>>>>;

pub struct SessionRunner<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Conversation name. Identifies this session in the User registry and
    /// is used to construct scope keys for consensus operations.
    pub(crate) conversation_name: String,
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
    pub(crate) phase_timer: PhaseTimer,
    /// Per-proposal auto-vote timers. The spawned task holds a clone of
    /// this `Arc` so it can self-clean on completion; coordinators use
    /// `cancel_auto_vote` / `cancel_all_auto_votes` to abort early.
    pub(crate) auto_vote_timers: AutoVoteTimers,
    /// Synchronous outbound transport (cloned from `User`). Per-session
    /// methods use `send_outbound` to wrap `DeliveryService::send` in
    /// `spawn_blocking`.
    #[allow(dead_code)] // wired in chunk-C phase 2; users land in subsequent waves
    pub(crate) transport: Arc<dyn crate::ds::DeliveryService>,
    /// Cached identity bytes (cloned from `User`). Used by per-session
    /// methods that need the local identity without re-walking the
    /// `Identity` trait.
    #[allow(dead_code)] // wired in chunk-C phase 2; users land in subsequent waves
    pub(crate) self_identity: Arc<[u8]>,
    /// Per-User instance UUID (cloned from `User`). Tagged on every
    /// outbound packet for self-message filtering.
    #[allow(dead_code)] // wired in chunk-C phase 2; users land in subsequent waves
    pub(crate) app_id: Arc<[u8]>,
    /// Per-session notification channel. Integrators subscribe via
    /// [`Self::subscribe`] and consume [`SessionEvent`]s for UI / audit.
    /// Fire-and-forget; no failure path back into the session.
    pub(crate) events: broadcast::Sender<SessionEvent>,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Build a fresh runner. Creator path passes `Some(mls)`; joiner
    /// path passes `None` and attaches the MLS service later via
    /// `handle.attach_mls`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        conversation_name: String,
        conversation: Conversation,
        mls: Option<CP::Mls>,
        state_machine: ConversationStateMachine,
        phase_timer: PhaseTimer,
        config: ConversationConfig,
        scoring: CP::Scoring,
        steward_list: CP::StewardList,
        consensus: PluginConsensus<P>,
        transport: Arc<dyn crate::ds::DeliveryService>,
        self_identity: Arc<[u8]>,
        app_id: Arc<[u8]>,
    ) -> Self {
        let (events, _initial_rx) = broadcast::channel(SESSION_EVENT_CAPACITY);
        Self {
            conversation_name,
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
            auto_vote_timers: Arc::new(Mutex::new(HashMap::new())),
            transport,
            self_identity,
            app_id,
            events,
        }
    }

    /// Borrow the conversation name. Used by per-session methods to build
    /// scope keys without threading the name through every signature.
    #[allow(dead_code)]
    pub(crate) fn conversation_name(&self) -> &str {
        &self.conversation_name
    }

    /// Subscribe to per-session [`SessionEvent`] notifications. Each call
    /// returns a fresh receiver; late subscribers miss earlier events.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    /// Emit a [`SessionEvent`] on the session's broadcast channel. Silently
    /// drops the event when there are no live subscribers — events are
    /// fire-and-forget.
    pub(crate) fn emit_event(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }

    /// Send an outbound packet via the session's transport. Wraps the
    /// synchronous [`crate::ds::DeliveryService::send`] in `spawn_blocking`
    /// so the async context isn't blocked. Mirrors `User::send_outbound`
    /// so session methods can send without going back through `User`.
    #[allow(dead_code)] // wired in chunk-C phase 2; callers land in subsequent waves
    pub(crate) async fn send_outbound(
        &self,
        packet: crate::ds::OutboundPacket,
    ) -> Result<String, crate::app::UserError> {
        let transport = std::sync::Arc::clone(&self.transport);
        let send_result = tokio::task::spawn_blocking(move || transport.send(packet))
            .await
            .expect("transport send task panicked");
        Ok(send_result?)
    }

    // ── Auto-vote timers ────────────────────────────────────────────

    /// Abort the auto-vote timer for `proposal_id` if one is registered.
    /// No-op otherwise.
    pub(crate) fn cancel_auto_vote(&self, proposal_id: u32) {
        if let Ok(mut timers) = self.auto_vote_timers.lock()
            && let Some(handle) = timers.remove(&proposal_id)
        {
            handle.abort();
        }
    }

    /// Abort every auto-vote timer registered on this runner. Called on
    /// conversation leave so no stale timers fire against a conversation we've left.
    pub(crate) fn cancel_all_auto_votes(&self) {
        if let Ok(mut timers) = self.auto_vote_timers.lock() {
            for (_, handle) in timers.drain() {
                handle.abort();
            }
        }
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
    ///
    /// Pipeline: consensus (~15s) + commit_inactivity + freeze (≈ commit/2)
    /// = ~1.5× commit-inactivity + consensus overhead. Use 3× for safety margin.
    pub(crate) fn is_pending_join_expired(&self) -> bool {
        self.handle.current_state() == ConversationState::PendingJoin
            && self
                .phase_timer
                .elapsed_since_anchor(self.handle.config.commit_inactivity_duration * 3)
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub(crate) fn is_freeze_timed_out(&self) -> bool {
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
    use super::*;
    use crate::core::Conversation;
    use crate::test_fixtures::{StubPluginsFactory, StubScoring, StubStewardList, UnusedMls};
    use std::time::Instant;

    use crate::core::DefaultConsensusPlugin;
    use crate::test_fixtures::make_test_consensus_service;

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
            Arc::new(crate::test_fixtures::UnusedTransport),
            Arc::from(&b"test-identity"[..]),
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
            Arc::new(crate::test_fixtures::UnusedTransport),
            Arc::from(&b"test-identity"[..]),
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
}
