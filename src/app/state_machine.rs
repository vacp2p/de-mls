//! Per-group FSM: PendingJoin → Working → Freezing → Selection → Reelection → Leaving.
use async_trait::async_trait;
use std::{
    fmt::Display,
    time::{Duration, Instant},
};
use tracing::info;

use crate::app::config::GroupConfig;

/// Notifies the integrator when a group transitions between [`GroupState`]
/// variants. Fires from the app layer only — core never calls it.
#[async_trait]
pub trait StateChangeHandler: Send + Sync {
    async fn on_state_changed(&self, group_name: &str, state: GroupState);
}

#[derive(Debug, Clone, PartialEq)]
pub enum GroupState {
    /// Waiting for the welcome after sending our key package.
    PendingJoin,
    /// Normal operation — chat and membership requests flow freely.
    Working,
    /// Collecting commit candidates for this epoch.
    Freezing,
    /// Deterministic selection across the buffered candidates.
    Selection,
    /// Steward list unusable; only emergency proposals accepted until a new
    /// election lands.
    Reelection,
    /// We asked to leave; waiting for our removal commit to arrive.
    Leaving,
}

impl Display for GroupState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            GroupState::PendingJoin => "PendingJoin",
            GroupState::Working => "Working",
            GroupState::Freezing => "Freezing",
            GroupState::Selection => "Selection",
            GroupState::Reelection => "Reelection",
            GroupState::Leaving => "Leaving",
        };
        write!(f, "{state}")
    }
}

/// What a freeze-timeout poll returned.
#[derive(Debug, PartialEq)]
pub enum FreezeTimeoutStatus {
    NotFreezing,
    StillFreezing,
    /// A candidate was selected and applied.
    Applied,
    /// Timeout elapsed without a valid candidate. `has_proposals = true`
    /// means approved work existed at timeout (steward fault); `false` is
    /// just an empty epoch.
    TimedOut {
        has_proposals: bool,
    },
}

#[derive(Debug, Clone)]
pub struct GroupStateMachine {
    state: GroupState,
    /// Meaning depends on `state`:
    /// - `PendingJoin`: time the join was initiated.
    /// - `Working`: time the first approved proposal arrived (drives the
    ///   steward-inactivity timer).
    /// - `Freezing`: time the freeze window started.
    /// - Other states: `None`.
    phase_timer: Option<Instant>,
    epoch_duration: Duration,
    freeze_duration: Duration,
    /// Short inactivity window used during recovery; caller of
    /// `check_steward_inactivity` picks which duration to apply.
    retry_inactivity_duration: Duration,
}

impl Default for GroupStateMachine {
    fn default() -> Self {
        Self::new_as_member()
    }
}

impl GroupStateMachine {
    pub fn new_as_member() -> Self {
        Self::new_as_member_with_config(GroupConfig::default())
    }

    pub fn new_as_member_with_config(config: GroupConfig) -> Self {
        Self {
            state: GroupState::Working,
            phase_timer: None,
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration,
            retry_inactivity_duration: config.retry_inactivity_duration,
        }
    }

    pub fn new_as_pending_join_with_config(config: GroupConfig) -> Self {
        Self {
            state: GroupState::PendingJoin,
            phase_timer: Some(Instant::now()),
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration,
            retry_inactivity_duration: config.retry_inactivity_duration,
        }
    }

    pub fn current_state(&self) -> GroupState {
        self.state.clone()
    }

    pub fn epoch_duration(&self) -> Duration {
        self.epoch_duration
    }

    pub fn freeze_duration(&self) -> Duration {
        self.freeze_duration
    }

    pub fn retry_inactivity_duration(&self) -> Duration {
        self.retry_inactivity_duration
    }

    /// Overwritten when the handle receives a `GroupSync` from the steward.
    pub fn update_timing(
        &mut self,
        epoch_duration: Duration,
        freeze_duration: Duration,
        retry_inactivity_duration: Duration,
    ) {
        self.epoch_duration = epoch_duration;
        self.freeze_duration = freeze_duration;
        self.retry_inactivity_duration = retry_inactivity_duration;
    }

    pub fn start_working(&mut self) {
        self.state = GroupState::Working;
        self.phase_timer = None;
        info!(state = "Working", "state transition");
    }

    pub fn start_freezing(&mut self) {
        self.state = GroupState::Freezing;
        self.phase_timer = Some(Instant::now());
        info!(state = "Freezing", "state transition");
    }

    /// Bypass the inactivity timer and enter Freezing immediately. Returns
    /// `true` on transition (only fires from `Working` or `Reelection`).
    pub fn force_freezing(&mut self) -> bool {
        match self.state {
            GroupState::Working | GroupState::Reelection => {
                self.start_freezing();
                true
            }
            _ => false,
        }
    }

    pub fn start_selection(&mut self) {
        self.state = GroupState::Selection;
        info!(state = "Selection", "state transition");
    }

    pub fn start_reelection(&mut self) {
        self.state = GroupState::Reelection;
        self.phase_timer = None;
        info!(state = "Reelection", "state transition");
    }

    /// Caller must ensure a valid transition. `User::leave_group` handles
    /// the PendingJoin and already-Leaving cases separately.
    pub fn start_leaving(&mut self) {
        self.state = GroupState::Leaving;
        info!(state = "Leaving", "state transition");
    }

    // ─────────────────────────── Pending Join ───────────────────────────

    /// `true` once 3× epoch-duration has passed in `PendingJoin` without a
    /// welcome — the join attempt is abandoned and local state torn down.
    pub fn is_pending_join_expired(&self) -> bool {
        if self.state != GroupState::PendingJoin {
            return false;
        }

        if let Some(started_at) = self.phase_timer {
            // Pipeline: consensus (~15s) + epoch_duration + freeze_duration (epoch/2)
            // = ~1.5× epoch + consensus overhead. Use 3× epoch for safety margin.
            let max_wait = self.epoch_duration * 3;
            if Instant::now() >= started_at + max_wait {
                return true;
            }
        }

        false
    }

    // ─────────────────────────── Freeze Timeout ───────────────────────────

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub fn is_freeze_timed_out(&self) -> bool {
        if self.state != GroupState::Freezing {
            return false;
        }

        if let Some(started_at) = self.phase_timer {
            return Instant::now() >= started_at + self.freeze_duration;
        }

        false
    }
    // ─────────────────────────── Proposal Timer (Member Inactivity) ───────────────────────────

    pub fn clear_proposal_timer(&mut self) {
        self.phase_timer = None;
    }

    /// Drives the "steward waited too long to commit" transition into
    /// `Freezing`. Call each poll tick; returns `true` exactly on the tick
    /// that transitions.
    ///
    /// - The timer anchors on the *first* approved proposal — a burst of
    ///   approvals doesn't reset it.
    /// - `start_working` clears the timer, so the next tick with leftover
    ///   approved work starts a fresh window (matters for auto-approved
    ///   self-leaves that survive a reelection round).
    /// - `inactivity_duration` is supplied by the caller (long during
    ///   normal operation, short during recovery).
    /// - No-op outside `Working`.
    pub fn check_steward_inactivity(
        &mut self,
        approved_proposals_count: usize,
        inactivity_duration: Duration,
    ) -> bool {
        if self.state != GroupState::Working || approved_proposals_count == 0 {
            return false;
        }

        let first_approved = match self.phase_timer {
            Some(t) => t,
            None => {
                self.phase_timer = Some(Instant::now());
                info!(
                    approved = approved_proposals_count,
                    inactivity_ms = inactivity_duration.as_millis() as u64,
                    "inactivity timer started"
                );
                return false;
            }
        };

        if Instant::now() < first_approved + inactivity_duration {
            return false;
        }

        self.start_freezing();
        info!(
            inactivity_ms = inactivity_duration.as_millis() as u64,
            approved = approved_proposals_count,
            "inactivity window elapsed, entering freeze"
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_join_timeout() {
        let config = GroupConfig::default();
        let mut state_machine = GroupStateMachine::new_as_pending_join_with_config(config.clone());
        assert!(!state_machine.is_pending_join_expired());

        // Backdate past the 3× epoch_duration expiration threshold.
        let past = config.epoch_duration * 3 + Duration::from_secs(1);
        state_machine.phase_timer = Some(Instant::now() - past);
        assert!(state_machine.is_pending_join_expired());
    }

    #[test]
    fn test_pending_join_not_expired_when_working() {
        // is_pending_join_expired is meaningful only in PendingJoin.
        let state_machine = GroupStateMachine::new_as_member();
        assert!(!state_machine.is_pending_join_expired());
    }

    #[test]
    fn test_freeze_timeout_not_in_freezing() {
        let state_machine = GroupStateMachine::new_as_member();
        // Not in Freezing → not timed out
        assert!(!state_machine.is_freeze_timed_out());
    }

    #[test]
    fn test_freeze_timeout_fresh_freezing() {
        let mut state_machine = GroupStateMachine::new_as_member();
        state_machine.start_freezing();
        // Just entered Freezing → not timed out yet
        assert!(!state_machine.is_freeze_timed_out());
    }

    #[test]
    fn test_freeze_timeout_expired() {
        let mut state_machine = GroupStateMachine::new_as_member();
        state_machine.start_freezing();
        // Backdate phase start well past freeze duration.
        state_machine.phase_timer = Some(Instant::now() - Duration::from_secs(30));
        assert!(state_machine.is_freeze_timed_out());
    }

    #[test]
    fn test_freeze_timeout_cleared_on_working() {
        let mut state_machine = GroupStateMachine::new_as_member();
        state_machine.start_freezing();
        assert!(state_machine.phase_timer.is_some());

        state_machine.start_working();
        assert!(state_machine.phase_timer.is_none());
        assert!(!state_machine.is_freeze_timed_out());
    }

    // ─────────────────────────── Proposal Timer Tests ───────────────────────────

    fn long_inactivity() -> Duration {
        Duration::from_secs(60)
    }

    fn short_inactivity() -> Duration {
        Duration::from_secs(5)
    }

    #[test]
    fn test_inactivity_timer_self_starts_on_first_check_with_approved() {
        let mut sm = GroupStateMachine::new_as_member();
        assert!(sm.phase_timer.is_none());

        // First call with approved work: timer starts, no transition yet.
        assert!(!sm.check_steward_inactivity(1, long_inactivity()));
        assert!(sm.phase_timer.is_some());
    }

    #[test]
    fn test_inactivity_timer_not_restarted_while_running() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.check_steward_inactivity(1, long_inactivity());
        let first_time = sm.phase_timer.unwrap();

        std::thread::sleep(Duration::from_millis(5));

        // Same-or-more approved → same timer.
        sm.check_steward_inactivity(2, long_inactivity());
        assert_eq!(sm.phase_timer.unwrap(), first_time);
    }

    #[test]
    fn test_steward_inactivity_triggers_freezing() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.phase_timer = Some(Instant::now() - Duration::from_secs(1));

        assert!(sm.check_steward_inactivity(1, Duration::from_millis(50)));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    #[test]
    fn test_check_inactivity_uses_caller_supplied_duration() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.phase_timer = Some(Instant::now() - Duration::from_millis(100));
        assert!(!sm.check_steward_inactivity(1, long_inactivity()));
        assert_eq!(sm.current_state(), GroupState::Working);

        assert!(sm.check_steward_inactivity(1, Duration::from_millis(50)));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    #[test]
    fn test_retry_inactivity_duration_threaded_from_config() {
        let config = GroupConfig {
            epoch_duration: long_inactivity(),
            retry_inactivity_duration: short_inactivity(),
            ..GroupConfig::default()
        };
        let sm = GroupStateMachine::new_as_member_with_config(config);
        assert_eq!(sm.epoch_duration(), long_inactivity());
        assert_eq!(sm.retry_inactivity_duration(), short_inactivity());
    }

    #[test]
    fn test_force_freezing_from_working() {
        let mut sm = GroupStateMachine::new_as_member();
        assert_eq!(sm.current_state(), GroupState::Working);
        assert!(sm.force_freezing());
        assert_eq!(sm.current_state(), GroupState::Freezing);
        assert!(sm.phase_timer.is_some());
    }

    #[test]
    fn test_force_freezing_from_reelection() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.start_reelection();
        assert!(sm.force_freezing());
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    #[test]
    fn test_force_freezing_noop_in_mid_cycle_or_terminal_states() {
        for state_setup in [
            |sm: &mut GroupStateMachine| sm.start_freezing(),
            |sm: &mut GroupStateMachine| sm.start_selection(),
            |sm: &mut GroupStateMachine| sm.start_leaving(),
        ] {
            let mut sm = GroupStateMachine::new_as_member();
            state_setup(&mut sm);
            let before = sm.current_state();
            assert!(!sm.force_freezing());
            assert_eq!(sm.current_state(), before);
        }
    }

    #[test]
    fn test_steward_inactivity_skips_if_already_freezing() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.phase_timer = Some(Instant::now() - Duration::from_secs(1));
        sm.start_freezing();

        // Already in Freezing — should not re-trigger
        assert!(!sm.check_steward_inactivity(1, Duration::from_millis(50)));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }
}
