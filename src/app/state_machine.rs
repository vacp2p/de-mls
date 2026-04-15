//! State machine for group lifecycle and freeze/commit flow management.
use async_trait::async_trait;
use std::{
    fmt::Display,
    time::{Duration, Instant},
};
use tracing::info;

use crate::app::config::GroupConfig;

/// Trait for handling state machine state changes.
///
/// This is an app-layer trait (not part of core API) for receiving
/// notifications when the group state changes.
#[async_trait]
pub trait StateChangeHandler: Send + Sync {
    /// Called when the group state changes.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `state` - The new state
    async fn on_state_changed(&self, group_name: &str, state: GroupState);
}

/// Represents the different states a group can be in during the commit lifecycle.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupState {
    /// Waiting for a welcome message after sending a key package.
    PendingJoin,
    /// Normal operation state - users can send any message freely.
    Working,
    /// Freeze window for collecting commit candidates.
    Freezing,
    /// Deterministic candidate selection phase.
    Selection,
    /// Emergency reelection phase (chat allowed, membership changes blocked).
    Reelection,
    /// User has requested to leave; waiting for the removal commit to arrive.
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

/// Result of checking freeze timeout status.
#[derive(Debug, PartialEq)]
pub enum FreezeTimeoutStatus {
    /// Not in Freezing state — nothing to check.
    NotFreezing,
    /// In Freezing state but timeout hasn't been reached yet.
    StillFreezing,
    /// Timeout reached, a candidate was selected and applied successfully.
    Applied,
    /// Timeout reached but no valid candidate was applied.
    /// `has_proposals` indicates if approved proposals existed at timeout
    /// (true = steward fault, false = empty epoch).
    TimedOut { has_proposals: bool },
}

/// State machine for managing group commit lifecycle.
#[derive(Debug, Clone)]
pub struct GroupStateMachine {
    /// Current state of the group.
    state: GroupState,
    /// Phase timer — meaning depends on current state:
    /// - `PendingJoin`: when join started (timeout after 2 × epoch_duration)
    /// - `Working`: when first proposal approved (steward inactivity after epoch_duration)
    /// - `Freezing`: when freeze started (selection after freeze_duration)
    /// - Other states: None
    phase_timer: Option<Instant>,
    /// Duration of each epoch (used for join timeout and inactivity detection).
    epoch_duration: Duration,
    /// Duration of the freeze phase before deterministic selection.
    freeze_duration: Duration,
}

impl Default for GroupStateMachine {
    fn default() -> Self {
        Self::new_as_member()
    }
}

impl GroupStateMachine {
    /// Create a new group state machine (not steward) with default config.
    pub fn new_as_member() -> Self {
        Self::new_as_member_with_config(GroupConfig::default())
    }

    /// Create a new group state machine (not steward) with custom config.
    pub fn new_as_member_with_config(config: GroupConfig) -> Self {
        Self {
            state: GroupState::Working,
            phase_timer: None,
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration,
        }
    }

    /// Create a new group state machine in PendingJoin state with custom config.
    pub fn new_as_pending_join_with_config(config: GroupConfig) -> Self {
        Self {
            state: GroupState::PendingJoin,
            phase_timer: Some(Instant::now()),
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration,
        }
    }

    /// Get the current state.
    pub fn current_state(&self) -> GroupState {
        self.state.clone()
    }

    /// Start working state.
    pub fn start_working(&mut self) {
        self.state = GroupState::Working;
        self.phase_timer = None;
        info!("[start_working] Transitioning to Working state");
    }

    /// Start freezing state.
    pub fn start_freezing(&mut self) {
        self.state = GroupState::Freezing;
        self.phase_timer = Some(Instant::now());
        info!("[start_freezing] Transitioning to Freezing state");
    }

    /// Start deterministic selection state.
    pub fn start_selection(&mut self) {
        self.state = GroupState::Selection;
        info!("[start_selection] Transitioning to Selection state");
    }

    /// Enter emergency reelection state.
    pub fn start_reelection(&mut self) {
        self.state = GroupState::Reelection;
        self.phase_timer = None;
        info!("[start_reelection] Transitioning to Reelection state");
    }

    /// Transition to Leaving state.
    ///
    /// Caller must ensure valid state transition.
    /// The `User::leave_group` method handles PendingJoin and Leaving states separately.
    pub fn start_leaving(&mut self) {
        self.state = GroupState::Leaving;
        info!("[start_leaving] Transitioning to Leaving state");
    }

    // ─────────────────────────── Pending Join ───────────────────────────

    /// Check if the pending join has expired (time-based).
    ///
    /// Expiration happens when ~2 epoch durations have passed since join attempt.
    /// If the member hasn't received a welcome by then, assume rejection.
    pub fn is_pending_join_expired(&self) -> bool {
        if self.state != GroupState::PendingJoin {
            return false;
        }

        if let Some(started_at) = self.phase_timer {
            let max_wait = self.epoch_duration * 2;
            if Instant::now() >= started_at + max_wait {
                return true;
            }
        }

        false
    }

    // ─────────────────────────── Freeze Timeout ───────────────────────────

    /// Check if the freeze window elapsed while in `Freezing`.
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

    /// Notify the state machine that a proposal was approved.
    ///
    /// If the approved count transitions from 0 to >0, start the inactivity timer.
    /// Additional proposals do NOT restart the timer.
    pub fn notify_proposal_approved(&mut self, before_count: usize, after_count: usize) {
        if before_count == 0 && after_count > 0 {
            self.phase_timer = Some(Instant::now());
            info!("[notify_proposal_approved] Inactivity timer started (0 → {after_count})");
        }
    }

    /// Clear the proposal inactivity timer.
    pub fn clear_proposal_timer(&mut self) {
        self.phase_timer = None;
    }

    /// Detect steward inactivity and transition to Freezing if needed.
    ///
    /// Returns `true` if transitioned to Freezing, `false` otherwise.
    /// Skips if:
    /// - not in Working state (already freezing or in another phase)
    /// - no approved proposals waiting for commit
    /// - epoch_duration hasn't elapsed since first proposal was approved
    pub fn check_steward_inactivity(&mut self, approved_proposals_count: usize) -> bool {
        if self.state != GroupState::Working {
            return false;
        }

        if approved_proposals_count == 0 {
            return false;
        }

        if let Some(first_approved) = self.phase_timer {
            if Instant::now() >= first_approved + self.epoch_duration {
                self.start_freezing();
                info!(
                    "[check_steward_inactivity] Steward inactivity detected after {:?} \
                     with {} approved proposals",
                    self.epoch_duration, approved_proposals_count
                );
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ProtocolConfig;

    #[test]
    fn test_state_machine_creation() {
        let state_machine = GroupStateMachine::new_as_member();
        assert_eq!(state_machine.current_state(), GroupState::Working);
    }

    #[test]
    fn test_state_machine_pending_join() {
        let state_machine =
            GroupStateMachine::new_as_pending_join_with_config(GroupConfig::default());
        assert_eq!(state_machine.current_state(), GroupState::PendingJoin);
        assert!(!state_machine.is_pending_join_expired());
    }

    #[test]
    fn test_pending_join_timeout() {
        let mut state_machine =
            GroupStateMachine::new_as_pending_join_with_config(GroupConfig::default());
        assert!(!state_machine.is_pending_join_expired());

        // Simulate time passing (~2 epochs) by backdating the start time
        state_machine.phase_timer = Some(Instant::now() - Duration::from_secs(120)); // Well past 2 epochs (60s)

        // Should expire after ~2 epoch durations
        assert!(state_machine.is_pending_join_expired());
    }

    #[test]
    fn test_pending_join_not_expired_when_working() {
        let state_machine = GroupStateMachine::new_as_member();
        assert_eq!(state_machine.current_state(), GroupState::Working);

        // Should not be expired when not in PendingJoin state
        assert!(!state_machine.is_pending_join_expired());
    }

    #[test]
    fn test_pending_join_to_working() {
        let mut state_machine =
            GroupStateMachine::new_as_pending_join_with_config(GroupConfig::default());
        assert_eq!(state_machine.current_state(), GroupState::PendingJoin);

        state_machine.start_working();
        assert_eq!(state_machine.current_state(), GroupState::Working);
    }

    #[test]
    fn test_leaving_state() {
        let mut state_machine = GroupStateMachine::new_as_member();
        assert_eq!(state_machine.current_state(), GroupState::Working);

        state_machine.start_leaving();
        assert_eq!(state_machine.current_state(), GroupState::Leaving);
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

    #[test]
    fn test_proposal_timer_starts_on_zero_to_one() {
        let mut sm = GroupStateMachine::new_as_member();
        assert!(sm.phase_timer.is_none());

        sm.notify_proposal_approved(0, 1);
        assert!(sm.phase_timer.is_some());
    }

    #[test]
    fn test_proposal_timer_no_restart_on_additional() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.notify_proposal_approved(0, 1);
        let first_time = sm.phase_timer.unwrap();

        // Simulate a small delay
        std::thread::sleep(Duration::from_millis(5));

        sm.notify_proposal_approved(1, 2);
        // Timer should NOT have been reset
        assert_eq!(sm.phase_timer.unwrap(), first_time);
    }

    #[test]
    fn test_proposal_timer_clears_on_working() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.notify_proposal_approved(0, 1);
        assert!(sm.phase_timer.is_some());

        sm.start_working();
        assert!(sm.phase_timer.is_none());
    }

    #[test]
    fn test_steward_inactivity_triggers_freezing() {
        let config = GroupConfig {
            epoch_duration: Duration::from_millis(50),
            freeze_duration: Duration::from_millis(25),
            protocol: ProtocolConfig::new(1, 5).unwrap(),
            ..GroupConfig::default()
        };
        let mut sm = GroupStateMachine::new_as_member_with_config(config);
        // Backdate the first proposal approval to well past epoch_duration
        sm.phase_timer = Some(Instant::now() - Duration::from_secs(1));

        assert!(sm.check_steward_inactivity(1));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    #[test]
    fn test_steward_inactivity_skips_if_already_freezing() {
        let config = GroupConfig {
            epoch_duration: Duration::from_millis(50),
            freeze_duration: Duration::from_millis(25),
            protocol: ProtocolConfig::new(1, 5).unwrap(),
            ..GroupConfig::default()
        };
        let mut sm = GroupStateMachine::new_as_member_with_config(config);
        sm.phase_timer = Some(Instant::now() - Duration::from_secs(1));
        sm.start_freezing();

        // Already in Freezing — should not re-trigger
        assert!(!sm.check_steward_inactivity(1));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }
}
