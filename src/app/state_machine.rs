//! State machine for steward epoch management and group operations.
use async_trait::async_trait;
use std::{
    fmt::Display,
    time::{Duration, Instant},
};
use tracing::info;

use crate::app::scheduler::DEFAULT_EPOCH_DURATION;

/// Configuration for a group's epoch behavior.
///
/// This struct is extensible for future per-group settings.
#[derive(Debug, Clone)]
pub struct GroupConfig {
    /// Duration of each epoch.
    pub epoch_duration: Duration,
    /// Duration of the freeze phase before deterministic selection.
    ///
    /// Defaults to `epoch_duration / 2`.
    pub freeze_duration: Duration,
    /// Whether subset commit candidates are allowed during deterministic selection.
    pub allow_subset_candidates: bool,
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            epoch_duration: DEFAULT_EPOCH_DURATION,
            freeze_duration: DEFAULT_EPOCH_DURATION / 2,
            allow_subset_candidates: false,
        }
    }
}

impl GroupConfig {
    /// Create a new config with custom epoch duration.
    pub fn with_epoch_duration(epoch_duration: Duration) -> Self {
        Self {
            epoch_duration,
            freeze_duration: epoch_duration / 2,
            allow_subset_candidates: false,
        }
    }

    /// Effective freeze duration: explicit value or `epoch_duration / 2`.
    pub fn freeze_duration(&self) -> Duration {
        self.freeze_duration
    }
}

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

/// Represents the different states a group can be in during the steward epoch flow.
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

/// State machine for managing group steward epoch flow.
#[derive(Debug, Clone)]
pub struct GroupStateMachine {
    /// Current state of the group.
    state: GroupState,
    /// Whether this user is the steward for this group.
    is_steward: bool,
    /// Timestamp when PendingJoin state was entered (for timeout).
    pending_join_started_at: Option<Instant>,
    /// Timestamp when freeze/waiting-like phase was entered.
    phase_started_at: Option<Instant>,
    /// Timestamp of the last epoch boundary (commit/welcome reception).
    /// Used by members to sync their epoch with the steward.
    last_epoch_boundary: Option<Instant>,
    /// Timestamp when the first proposal was approved (going from 0 → 1+).
    /// Used by members to detect steward inactivity (Path B).
    first_proposal_approved_at: Option<Instant>,
    /// Duration of each epoch.
    epoch_duration: Duration,
    /// Freeze window duration before selection.
    freeze_duration: Duration,
    /// Whether subset commit candidates are allowed during deterministic selection.
    allow_subset_candidates: bool,
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
            is_steward: false,
            pending_join_started_at: None,
            phase_started_at: None,
            last_epoch_boundary: None,
            first_proposal_approved_at: None,
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration(),
            allow_subset_candidates: config.allow_subset_candidates,
        }
    }

    /// Create a new group state machine as steward with default config.
    pub fn new_as_steward() -> Self {
        Self::new_as_steward_with_config(GroupConfig::default())
    }

    /// Create a new group state machine as steward with custom config.
    pub fn new_as_steward_with_config(config: GroupConfig) -> Self {
        Self {
            state: GroupState::Working,
            is_steward: true,
            pending_join_started_at: None,
            phase_started_at: None,
            last_epoch_boundary: None,
            first_proposal_approved_at: None,
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration(),
            allow_subset_candidates: config.allow_subset_candidates,
        }
    }

    /// Create a new group state machine in PendingJoin state with default config.
    pub fn new_as_pending_join() -> Self {
        Self::new_as_pending_join_with_config(GroupConfig::default())
    }

    /// Create a new group state machine in PendingJoin state with custom config.
    pub fn new_as_pending_join_with_config(config: GroupConfig) -> Self {
        Self {
            state: GroupState::PendingJoin,
            is_steward: false,
            pending_join_started_at: Some(Instant::now()),
            phase_started_at: None,
            last_epoch_boundary: None,
            first_proposal_approved_at: None,
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration(),
            allow_subset_candidates: config.allow_subset_candidates,
        }
    }

    /// Get the current state.
    pub fn current_state(&self) -> GroupState {
        self.state.clone()
    }

    /// Check if this is a steward state machine.
    pub fn is_steward(&self) -> bool {
        self.is_steward
    }

    /// Whether subset commit candidates are allowed during deterministic selection.
    pub fn allow_subset_candidates(&self) -> bool {
        self.allow_subset_candidates
    }

    /// Start working state.
    pub fn start_working(&mut self) {
        self.state = GroupState::Working;
        self.phase_started_at = None;
        self.first_proposal_approved_at = None;
        info!("[start_working] Transitioning to Working state");
    }

    /// Start freezing state.
    pub fn start_freezing(&mut self) {
        self.state = GroupState::Freezing;
        self.phase_started_at = Some(Instant::now());
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
        self.phase_started_at = None;
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

        if let Some(started_at) = self.pending_join_started_at {
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

        if let Some(started_at) = self.phase_started_at {
            return Instant::now() >= started_at + self.freeze_duration;
        }

        false
    }

    // ─────────────────────────── Epoch Synchronization ───────────────────────────

    /// Sync the epoch boundary to now.
    /// Called when a commit or welcome (for joining) is received.
    /// This is the synchronization point between steward and member epochs.
    pub fn sync_epoch_boundary(&mut self) {
        self.last_epoch_boundary = Some(Instant::now());
        info!("[sync_epoch_boundary] Epoch boundary synchronized");
    }

    /// Get the time until the next expected epoch boundary.
    /// Returns `None` if no epoch boundary has been set yet.
    pub fn time_until_next_boundary(&self) -> Option<Duration> {
        self.last_epoch_boundary.map(|last| {
            let expected = last + self.epoch_duration;
            expected.saturating_duration_since(Instant::now())
        })
    }

    // ─────────────────────────── Proposal Timer (Member Inactivity) ───────────────────────────

    /// Notify the state machine that a proposal was approved.
    ///
    /// If the approved count transitions from 0 to >0, start the inactivity timer.
    /// Additional proposals do NOT restart the timer.
    pub fn notify_proposal_approved(&mut self, before_count: usize, after_count: usize) {
        if before_count == 0 && after_count > 0 {
            self.first_proposal_approved_at = Some(Instant::now());
            info!("[notify_proposal_approved] Inactivity timer started (0 → {after_count})");
        }
    }

    /// Clear the proposal inactivity timer.
    pub fn clear_proposal_timer(&mut self) {
        self.first_proposal_approved_at = None;
    }

    /// Check if the steward has been inactive long enough to trigger Freezing.
    ///
    /// For non-steward members in `Working` state: if `first_proposal_approved_at`
    /// is set AND `elapsed >= epoch_duration` AND `approved_proposals_count > 0`,
    /// transition to Freezing and return `true`.
    pub fn check_steward_inactivity(&mut self, approved_proposals_count: usize) -> bool {
        if self.is_steward {
            return false;
        }

        if self.state != GroupState::Working {
            return false;
        }

        if approved_proposals_count == 0 {
            return false;
        }

        if let Some(first_approved) = self.first_proposal_approved_at {
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

    // ─────────────────────────── Steward Operations ───────────────────────────

    /// Start steward epoch with state validation.
    /// # Errors
    /// - If not in Working state
    /// - If not a steward
    pub fn start_steward_epoch(&mut self) -> Result<(), StateMachineError> {
        if self.state != GroupState::Working {
            return Err(StateMachineError::InvalidTransition {
                from: self.state.to_string(),
                to: "Freezing".to_string(),
            });
        }

        if !self.is_steward {
            return Err(StateMachineError::NotSteward);
        }

        self.start_freezing();
        Ok(())
    }
}

/// Errors from state machine operations.
#[derive(Debug, thiserror::Error)]
pub enum StateMachineError {
    /// Invalid state transition attempted.
    #[error("Invalid state transition from {from} to {to}")]
    InvalidTransition { from: String, to: String },

    /// Operation requires steward status.
    #[error("Not a steward")]
    NotSteward,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_machine_creation() {
        let state_machine = GroupStateMachine::new_as_member();
        assert_eq!(state_machine.current_state(), GroupState::Working);
        assert!(!state_machine.is_steward());
    }

    #[test]
    fn test_state_machine_as_steward() {
        let state_machine = GroupStateMachine::new_as_steward();
        assert_eq!(state_machine.current_state(), GroupState::Working);
        assert!(state_machine.is_steward());
    }

    #[test]
    fn test_state_machine_pending_join() {
        let state_machine = GroupStateMachine::new_as_pending_join();
        assert_eq!(state_machine.current_state(), GroupState::PendingJoin);
        assert!(!state_machine.is_steward());
        assert!(!state_machine.is_pending_join_expired());
    }

    #[test]
    fn test_pending_join_timeout() {
        let mut state_machine = GroupStateMachine::new_as_pending_join();
        assert!(!state_machine.is_pending_join_expired());

        // Simulate time passing (~2 epochs) by backdating the start time
        state_machine.pending_join_started_at = Some(Instant::now() - Duration::from_secs(120)); // Well past 2 epochs (60s)

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
        let mut state_machine = GroupStateMachine::new_as_pending_join();
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
        state_machine.phase_started_at = Some(Instant::now() - Duration::from_secs(30));
        assert!(state_machine.is_freeze_timed_out());
    }

    #[test]
    fn test_freeze_timeout_cleared_on_working() {
        let mut state_machine = GroupStateMachine::new_as_member();
        state_machine.start_freezing();
        assert!(state_machine.phase_started_at.is_some());

        state_machine.start_working();
        assert!(state_machine.phase_started_at.is_none());
        assert!(!state_machine.is_freeze_timed_out());
    }

    // ─────────────────────────── Proposal Timer Tests ───────────────────────────

    #[test]
    fn test_proposal_timer_starts_on_zero_to_one() {
        let mut sm = GroupStateMachine::new_as_member();
        assert!(sm.first_proposal_approved_at.is_none());

        sm.notify_proposal_approved(0, 1);
        assert!(sm.first_proposal_approved_at.is_some());
    }

    #[test]
    fn test_proposal_timer_no_restart_on_additional() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.notify_proposal_approved(0, 1);
        let first_time = sm.first_proposal_approved_at.unwrap();

        // Simulate a small delay
        std::thread::sleep(Duration::from_millis(5));

        sm.notify_proposal_approved(1, 2);
        // Timer should NOT have been reset
        assert_eq!(sm.first_proposal_approved_at.unwrap(), first_time);
    }

    #[test]
    fn test_proposal_timer_clears_on_working() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.notify_proposal_approved(0, 1);
        assert!(sm.first_proposal_approved_at.is_some());

        sm.start_working();
        assert!(sm.first_proposal_approved_at.is_none());
    }

    #[test]
    fn test_steward_inactivity_triggers_freezing() {
        let config = GroupConfig {
            epoch_duration: Duration::from_millis(50),
            freeze_duration: Duration::from_millis(25),
            allow_subset_candidates: false,
        };
        let mut sm = GroupStateMachine::new_as_member_with_config(config);
        // Backdate the first proposal approval to well past epoch_duration
        sm.first_proposal_approved_at = Some(Instant::now() - Duration::from_secs(1));

        assert!(sm.check_steward_inactivity(1));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    #[test]
    fn test_steward_inactivity_skips_steward() {
        let config = GroupConfig {
            epoch_duration: Duration::from_millis(50),
            freeze_duration: Duration::from_millis(25),
            allow_subset_candidates: false,
        };
        let mut sm = GroupStateMachine::new_as_steward_with_config(config);
        sm.first_proposal_approved_at = Some(Instant::now() - Duration::from_secs(1));

        // Steward should always return false
        assert!(!sm.check_steward_inactivity(1));
        assert_eq!(sm.current_state(), GroupState::Working);
    }
}
