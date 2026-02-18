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
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            epoch_duration: DEFAULT_EPOCH_DURATION,
        }
    }
}

impl GroupConfig {
    /// Create a new config with custom epoch duration.
    pub fn with_epoch_duration(epoch_duration: Duration) -> Self {
        Self { epoch_duration }
    }
}

/// Trait for handling state machine state changes.
///
/// This is an app-layer trait (not part of core API) for receiving
/// notifications when the group state changes.
#[async_trait]
pub trait StateChangeHandler: Send + Sync {
    /// Called when the group state changes (PendingJoin, Working, Waiting, Leaving).
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
    /// Waiting state during steward epoch - only steward can send BATCH_PROPOSALS_MESSAGE.
    Waiting,
    /// User has requested to leave; waiting for the removal commit to arrive.
    Leaving,
}

impl Display for GroupState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            GroupState::PendingJoin => "PendingJoin",
            GroupState::Working => "Working",
            GroupState::Waiting => "Waiting",
            GroupState::Leaving => "Leaving",
        };
        write!(f, "{state}")
    }
}

/// Result of checking commit timeout status.
#[derive(Debug, PartialEq)]
pub enum CommitTimeoutStatus {
    /// Not in Waiting state — nothing to check.
    NotWaiting,
    /// In Waiting state but timeout hasn't been reached yet.
    StillWaiting,
    /// Timeout reached and state reverted to Working.
    /// `has_proposals` indicates if approved proposals still existed (steward fault).
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
    /// Timestamp when Waiting state was entered (for commit timeout).
    waiting_started_at: Option<Instant>,
    /// Timestamp of the last epoch boundary (commit/welcome reception).
    /// Used by members to sync their epoch with the steward.
    last_epoch_boundary: Option<Instant>,
    /// Duration of each epoch.
    epoch_duration: Duration,
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
            waiting_started_at: None,
            last_epoch_boundary: None,
            epoch_duration: config.epoch_duration,
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
            waiting_started_at: None,
            last_epoch_boundary: None,
            epoch_duration: config.epoch_duration,
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
            waiting_started_at: None,
            last_epoch_boundary: None,
            epoch_duration: config.epoch_duration,
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

    /// Set steward status.
    pub fn set_steward(&mut self, is_steward: bool) {
        self.is_steward = is_steward;
    }

    /// Start working state.
    pub fn start_working(&mut self) {
        self.state = GroupState::Working;
        self.waiting_started_at = None;
        info!("[start_working] Transitioning to Working state");
    }

    /// Start waiting state.
    pub fn start_waiting(&mut self) {
        self.state = GroupState::Waiting;
        self.waiting_started_at = Some(Instant::now());
        info!("[start_waiting] Transitioning to Waiting state");
    }

    /// Transition to Leaving state.
    ///
    /// Caller must ensure valid state transition (typically from Working or Waiting).
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

    // ─────────────────────────── Commit Timeout ───────────────────────────

    /// Check if the commit has timed out while in Waiting state.
    ///
    /// Returns `true` if the member has been in Waiting for longer than
    /// `epoch_duration / 2` without receiving a commit from the steward.
    pub fn is_commit_timed_out(&self) -> bool {
        if self.state != GroupState::Waiting {
            return false;
        }

        if let Some(started_at) = self.waiting_started_at {
            let timeout = self.epoch_duration / 2;
            if Instant::now() >= started_at + timeout {
                return true;
            }
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

    /// Check if we've reached the expected epoch boundary and should enter Waiting.
    ///
    /// Called by the member epoch timer. Returns `true` if entering Waiting state
    /// (meaning a commit timeout should be started).
    ///
    /// # Arguments
    /// * `approved_proposals_count` - Number of approved proposals waiting for commit
    ///
    /// # Returns
    /// `true` if transitioned to Waiting state, `false` otherwise.
    pub fn check_epoch_boundary(&mut self, approved_proposals_count: usize) -> bool {
        // Skip if steward (they manage their own epoch) or not initialized
        if self.is_steward {
            return false;
        }

        // Skip if in PendingJoin or Leaving state
        if self.state == GroupState::PendingJoin || self.state == GroupState::Leaving {
            return false;
        }

        // Already Waiting for commit — don't re-enter or reset the timeout timer.
        // The commit timeout mechanism handles this case.
        if self.state == GroupState::Waiting {
            return false;
        }

        // Check if we've reached the expected boundary
        if let Some(last_boundary) = self.last_epoch_boundary {
            let expected = last_boundary + self.epoch_duration;
            if Instant::now() >= expected {
                // Advance boundary for next epoch
                self.last_epoch_boundary = Some(expected);

                if approved_proposals_count > 0 {
                    // We have approved proposals → freeze and wait for commit
                    self.start_waiting();
                    info!(
                        "[check_epoch_boundary] Entering Waiting state with {} approved proposals",
                        approved_proposals_count
                    );
                    return true;
                }
                // No proposals → stay Working, just advanced the boundary
                info!("[check_epoch_boundary] No proposals, staying in Working state");
            }
        }
        // No last_epoch_boundary set means we haven't synced yet (first epoch after join)
        // Just wait for the first commit to sync

        false
    }

    /// Get the time until the next expected epoch boundary.
    /// Returns `None` if no epoch boundary has been set yet.
    pub fn time_until_next_boundary(&self) -> Option<Duration> {
        self.last_epoch_boundary.map(|last| {
            let expected = last + self.epoch_duration;
            expected.saturating_duration_since(Instant::now())
        })
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
                to: "Waiting".to_string(),
            });
        }

        if !self.is_steward {
            return Err(StateMachineError::NotSteward);
        }

        self.start_waiting();
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
    fn test_epoch_sync_and_boundary_check() {
        let mut state_machine = GroupStateMachine::new_as_member();

        // No boundary set initially
        assert!(state_machine.time_until_next_boundary().is_none());

        // Sync epoch boundary
        state_machine.sync_epoch_boundary();
        assert!(state_machine.time_until_next_boundary().is_some());

        // Immediately after sync, boundary not reached
        assert!(!state_machine.check_epoch_boundary(5));
        assert_eq!(state_machine.current_state(), GroupState::Working);
    }

    #[test]
    fn test_epoch_boundary_with_no_proposals() {
        let mut state_machine = GroupStateMachine::new_as_member();
        // Simulate past epoch boundary
        state_machine.last_epoch_boundary = Some(Instant::now() - Duration::from_secs(60));

        // No proposals → stay Working
        assert!(!state_machine.check_epoch_boundary(0));
        assert_eq!(state_machine.current_state(), GroupState::Working);
    }

    #[test]
    fn test_epoch_boundary_with_proposals() {
        let mut state_machine = GroupStateMachine::new_as_member();
        // Simulate past epoch boundary
        state_machine.last_epoch_boundary = Some(Instant::now() - Duration::from_secs(60));

        // Has proposals → enter Waiting
        assert!(state_machine.check_epoch_boundary(3));
        assert_eq!(state_machine.current_state(), GroupState::Waiting);
    }

    #[test]
    fn test_steward_skips_epoch_boundary_check() {
        let mut state_machine = GroupStateMachine::new_as_steward();
        state_machine.last_epoch_boundary = Some(Instant::now() - Duration::from_secs(60));

        // Steward should not enter Waiting via check_epoch_boundary
        assert!(!state_machine.check_epoch_boundary(5));
        assert_eq!(state_machine.current_state(), GroupState::Working);
    }

    #[test]
    fn test_commit_timeout_not_in_waiting() {
        let state_machine = GroupStateMachine::new_as_member();
        // Not in Waiting → not timed out
        assert!(!state_machine.is_commit_timed_out());
    }

    #[test]
    fn test_commit_timeout_fresh_waiting() {
        let mut state_machine = GroupStateMachine::new_as_member();
        state_machine.start_waiting();
        // Just entered Waiting → not timed out yet
        assert!(!state_machine.is_commit_timed_out());
    }

    #[test]
    fn test_commit_timeout_expired() {
        let mut state_machine = GroupStateMachine::new_as_member();
        state_machine.start_waiting();
        // Backdate waiting_started_at to well past epoch_duration/2 (15s for default 30s epoch)
        state_machine.waiting_started_at = Some(Instant::now() - Duration::from_secs(30));
        assert!(state_machine.is_commit_timed_out());
    }

    #[test]
    fn test_commit_timeout_cleared_on_working() {
        let mut state_machine = GroupStateMachine::new_as_member();
        state_machine.start_waiting();
        assert!(state_machine.waiting_started_at.is_some());

        state_machine.start_working();
        assert!(state_machine.waiting_started_at.is_none());
        assert!(!state_machine.is_commit_timed_out());
    }

    #[test]
    fn test_check_epoch_boundary_skips_when_already_waiting() {
        let mut state_machine = GroupStateMachine::new_as_member();
        state_machine.last_epoch_boundary = Some(Instant::now() - Duration::from_secs(60));

        // First call: enters Waiting
        assert!(state_machine.check_epoch_boundary(3));
        assert_eq!(state_machine.current_state(), GroupState::Waiting);

        // Advance boundary past next epoch
        state_machine.last_epoch_boundary = Some(Instant::now() - Duration::from_secs(60));

        // Second call while still Waiting: should NOT re-enter (returns false)
        assert!(!state_machine.check_epoch_boundary(3));
        assert_eq!(state_machine.current_state(), GroupState::Waiting);
    }
}
