//! State machine for steward epoch management and group operations.
//!
//! This module implements a state machine that manages the lifecycle of steward epochs,
//! proposal collection, voting, and application.
//!
//! # States
//!
//! - **Working**: Normal operation state where users can send any message freely
//! - **Waiting**: Steward epoch state where only steward can send BATCH_PROPOSALS_MESSAGE
//! - **Voting**: Voting state where everyone can send VOTE/USER_VOTE
//! - **ConsensusReached**: Consensus achieved, waiting for steward to send batch proposals
//!
//! # State Transitions
//!
//! ```text
//! Working -- start_steward_epoch() --> Waiting (if proposals exist)
//! Working -- start_steward_epoch() --> Working (if no proposals, returns 0)
//! Waiting -- start_voting() --> Voting
//! Voting -- complete_voting(true) --> ConsensusReached
//! Voting -- complete_voting(false) --> Working
//! ConsensusReached -- start_waiting_after_consensus() --> Waiting
//! Waiting -- handle_yes_vote() --> Working
//! ```

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
    /// Voting state - everyone can send VOTE/USER_VOTE, only steward can send VOTE_PAYLOAD/PROPOSAL.
    Voting,
    /// Consensus reached state - consensus achieved, waiting for steward to send batch proposals.
    ConsensusReached,
}

impl Display for GroupState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            GroupState::Working => "Working",
            GroupState::Waiting => "Waiting",
            GroupState::Voting => "Voting",
            GroupState::ConsensusReached => "ConsensusReached",
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
        Self::new()
    }
}

impl GroupStateMachine {
    /// Create a new group state machine (not steward).
    pub fn new() -> Self {
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
            GroupState::Voting => match message_type {
                message_types::VOTE => true,
                message_types::USER_VOTE => true,
                message_types::VOTE_PAYLOAD => is_steward,
                message_types::PROPOSAL => is_steward,
                _ => false,
            },
            GroupState::ConsensusReached => {
                matches!(message_type, message_types::BATCH_PROPOSALS_MESSAGE if is_steward && has_proposals)
            }
        }
    }

    /// Start voting on proposals.
    ///
    /// # Preconditions
    /// - Cannot be called from Voting state
    pub fn start_voting(&mut self) -> Result<(), StateMachineError> {
        if self.state == GroupState::Voting {
            return Err(StateMachineError::InvalidTransition {
                from: self.state.to_string(),
                to: "Voting".to_string(),
            });
        }
        self.state = GroupState::Voting;
        Ok(())
    }

    /// Complete voting and update state based on result.
    ///
    /// # Arguments
    /// * `vote_result` - `true` for YES vote, `false` for NO vote
    ///
    /// # State Transitions
    /// - Vote YES: Voting → ConsensusReached
    /// - Vote NO: Voting → Working
    pub fn complete_voting(&mut self, vote_result: bool) -> Result<(), StateMachineError> {
        if self.state != GroupState::Voting {
            return Err(StateMachineError::InvalidTransition {
                from: self.state.to_string(),
                to: if vote_result {
                    "ConsensusReached"
                } else {
                    "Working"
                }
                .to_string(),
            });
        }

        if vote_result {
            info!("[complete_voting]: Vote YES, transitioning to ConsensusReached state");
            self.state = GroupState::ConsensusReached;
        } else {
            info!("[complete_voting]: Vote NO, transitioning to Working state");
            self.state = GroupState::Working;
        }

        Ok(())
    }

    /// Start consensus reached state.
    pub fn start_consensus_reached(&mut self) {
        self.state = GroupState::ConsensusReached;
        info!("[start_consensus_reached] Transitioning to ConsensusReached state");
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

    /// Start steward epoch with validation.
    ///
    /// # Arguments
    /// * `proposal_count` - Number of proposals available
    ///
    /// # Returns
    /// - `Ok(proposal_count)` if transitioned to Waiting
    /// - `Ok(0)` if no proposals (stays in Working)
    ///
    /// # Errors
    /// - If not in Working state
    /// - If not a steward
    pub fn start_steward_epoch(
        &mut self,
        proposal_count: usize,
    ) -> Result<usize, StateMachineError> {
        if self.state != GroupState::Working {
            return Err(StateMachineError::InvalidTransition {
                from: self.state.to_string(),
                to: "Waiting".to_string(),
            });
        }

        if !self.is_steward {
            return Err(StateMachineError::NotSteward);
        }

        if proposal_count == 0 {
            Ok(0)
        } else {
            self.state = GroupState::Waiting;
            Ok(proposal_count)
        }
    }

    /// Handle YES vote completion.
    ///
    /// # Preconditions
    /// - Must be in ConsensusReached or Waiting state
    pub fn handle_yes_vote(&mut self) -> Result<(), StateMachineError> {
        if self.state != GroupState::ConsensusReached && self.state != GroupState::Waiting {
            return Err(StateMachineError::InvalidTransition {
                from: self.state.to_string(),
                to: "Working".to_string(),
            });
        }
        self.state = GroupState::Working;
        Ok(())
    }

    /// Start waiting state after consensus is reached.
    ///
    /// # Preconditions
    /// - Must be in ConsensusReached state
    pub fn start_waiting_after_consensus(&mut self) -> Result<(), StateMachineError> {
        if self.state != GroupState::ConsensusReached {
            return Err(StateMachineError::InvalidTransition {
                from: self.state.to_string(),
                to: "Waiting".to_string(),
            });
        }
        self.state = GroupState::Waiting;
        info!(
            "[start_waiting_after_consensus] Transitioning from ConsensusReached to Waiting state"
        );
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
        let state_machine = GroupStateMachine::new();
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
    fn test_state_transitions() {
        let mut state_machine = GroupStateMachine::new_as_steward();

        // Start steward epoch with proposals
        let result = state_machine.start_steward_epoch(2);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 2);
        assert_eq!(state_machine.current_state(), GroupState::Waiting);

        // Start voting
        assert!(state_machine.start_voting().is_ok());
        assert_eq!(state_machine.current_state(), GroupState::Voting);

        // Complete voting with YES
        assert!(state_machine.complete_voting(true).is_ok());
        assert_eq!(state_machine.current_state(), GroupState::ConsensusReached);

        // Start waiting after consensus
        assert!(state_machine.start_waiting_after_consensus().is_ok());
        assert_eq!(state_machine.current_state(), GroupState::Waiting);

        // Handle YES vote
        assert!(state_machine.handle_yes_vote().is_ok());
        assert_eq!(state_machine.current_state(), GroupState::Working);
    }

    #[test]
    fn test_no_proposals() {
        let mut state_machine = GroupStateMachine::new_as_steward();

        // Start steward epoch with no proposals
        let result = state_machine.start_steward_epoch(0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
        assert_eq!(state_machine.current_state(), GroupState::Working);
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
        let _ = state_machine.start_steward_epoch(1);

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

        // Move to Voting
        let _ = state_machine.start_voting();

        // Voting state
        assert!(state_machine.can_send_message_type(false, false, message_types::VOTE));
        assert!(state_machine.can_send_message_type(false, false, message_types::USER_VOTE));
        assert!(state_machine.can_send_message_type(true, false, message_types::VOTE_PAYLOAD));
        assert!(!state_machine.can_send_message_type(false, false, message_types::VOTE_PAYLOAD));
    }
}
