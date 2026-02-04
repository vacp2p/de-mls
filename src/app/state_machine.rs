//! State machine for steward epoch management and group operations.
use std::fmt::Display;
use tracing::info;

use crate::core::message_types;

/// Represents the different states a group can be in during the steward epoch flow.
#[derive(Debug, Clone, PartialEq)]
pub enum GroupState {
    /// Normal operation state - users can send any message freely.
    Working,
    /// Waiting state during steward epoch - only steward can send BATCH_PROPOSALS_MESSAGE.
    Waiting,
}

impl Display for GroupState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            GroupState::Working => "Working",
            GroupState::Waiting => "Waiting",
        };
        write!(f, "{state}")
    }
}

/// State machine for managing group steward epoch flow.
#[derive(Debug, Clone)]
pub struct GroupStateMachine {
    /// Current state of the group.
    state: GroupState,
    /// Whether this user is the steward for this group.
    is_steward: bool,
}

impl Default for GroupStateMachine {
    fn default() -> Self {
        Self::new_as_member()
    }
}

impl GroupStateMachine {
    /// Create a new group state machine (not steward).
    pub fn new_as_member() -> Self {
        Self {
            state: GroupState::Working,
            is_steward: false,
        }
    }

    /// Create a new group state machine as steward.
    pub fn new_as_steward() -> Self {
        Self {
            state: GroupState::Working,
            is_steward: true,
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

    /// Check if a specific message type can be sent in the current state.
    ///
    /// # Arguments
    /// * `is_steward` - Whether the sender is a steward
    /// * `has_proposals` - Whether there are proposals available
    /// * `message_type` - The type of message to check
    ///
    /// # Returns
    /// `true` if the message can be sent, `false` otherwise.
    pub fn can_send_message_type(
        &self,
        is_steward: bool,
        has_proposals: bool,
        message_type: &str,
    ) -> bool {
        match self.state {
            GroupState::Working => true,
            GroupState::Waiting => {
                matches!(message_type, message_types::BATCH_PROPOSALS_MESSAGE if is_steward && has_proposals)
            }
        }
    }

    /// Start working state.
    pub fn start_working(&mut self) {
        self.state = GroupState::Working;
        info!("[start_working] Transitioning to Working state");
    }

    /// Start waiting state.
    pub fn start_waiting(&mut self) {
        self.state = GroupState::Waiting;
        info!("[start_waiting] Transitioning to Waiting state");
    }

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
    fn test_message_permissions() {
        let mut state_machine = GroupStateMachine::new_as_steward();

        // Working state - all messages allowed
        assert!(state_machine.can_send_message_type(
            false,
            false,
            message_types::CONVERSATION_MESSAGE
        ));

        // Move to Waiting
        let _ = state_machine.start_steward_epoch();

        // Waiting state - only batch proposals for steward with proposals
        assert!(state_machine.can_send_message_type(
            true,
            true,
            message_types::BATCH_PROPOSALS_MESSAGE
        ));
        assert!(!state_machine.can_send_message_type(
            false,
            true,
            message_types::BATCH_PROPOSALS_MESSAGE
        ));
        assert!(!state_machine.can_send_message_type(
            true,
            false,
            message_types::BATCH_PROPOSALS_MESSAGE
        ));
    }
}
