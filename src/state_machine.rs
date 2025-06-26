//! State machine for steward epoch management and group operations.
//!
//! This module implements a state machine that manages the lifecycle of steward epochs,
//! proposal collection, voting, and application. The state machine ensures proper
//! transitions and enforces permissions at each state.
//!
//! # States
//!
//! - **Working**: Normal operation state where users can send messages freely
//! - **Waiting**: Steward epoch state where only steward can send messages with proposals
//! - **Voting**: Transitional state during voting process where no messages are allowed
//!
//! # State Transitions
//!
//! ```text
//! Working --start_steward_epoch()--> Waiting (if proposals exist)
//! Working --start_steward_epoch()--> Working (if no proposals)
//! Waiting --start_voting()---------> Voting
//! Voting --complete_voting(true)--> Waiting (vote passed)
//! Voting --complete_voting(false)-> Working (vote failed)
//! Waiting --apply_proposals_and_complete()--> Working
//! ```

use std::fmt::Display;

use crate::steward::Steward;
use crate::{steward::GroupUpdateRequest, GroupError};

/// Represents the different states a group can be in during the steward epoch flow
#[derive(Debug, Clone, PartialEq)]
pub enum GroupState {
    /// Normal operation state - users can send messages freely
    Working,
    /// Waiting state during steward epoch - only steward can send messages with proposals
    Waiting,
    /// Transitional state during voting process
    Voting,
}

impl Display for GroupState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            GroupState::Working => "Working - Normal operation",
            GroupState::Waiting => "Waiting - Steward epoch active",
            GroupState::Voting => "Voting - Vote in progress",
        };
        write!(f, "{}", state)
    }
}

/// State machine for managing group steward epoch flow
#[derive(Debug, Clone)]
pub struct GroupStateMachine {
    /// Current state of the group
    state: GroupState,
    /// Optional steward for epoch management
    steward: Option<Steward>,
}

impl GroupStateMachine {
    /// Create a new group state machine
    pub fn new() -> Self {
        Self {
            state: GroupState::Working,
            steward: None,
        }
    }

    /// Create a new group state machine with steward
    pub fn new_with_steward() -> Self {
        Self {
            state: GroupState::Working,
            steward: Some(Steward::new()),
        }
    }

    /// Get the current state
    pub fn current_state(&self) -> GroupState {
        self.state.clone()
    }

    /// Check if a message can be sent in the current state
    pub fn can_send_message(&self, is_steward: bool, has_proposals: bool) -> bool {
        match self.state {
            GroupState::Working => true, // Anyone can send messages in working state
            GroupState::Waiting => is_steward && has_proposals, // Only steward with proposals can send
            GroupState::Voting => false, // No one can send messages during voting
        }
    }

    /// Start a new steward epoch, transitioning to Waiting state
    pub async fn start_steward_epoch(&mut self) -> Result<(), GroupError> {
        println!(
            "State machine: start_steward_epoch called, current state: {:?}",
            self.state
        );
        if self.state != GroupState::Working {
            println!(
                "State machine: Invalid state transition from {:?} to Waiting",
                self.state
            );
            return Err(GroupError::InvalidStateTransition);
        }

        self.state = GroupState::Waiting;
        println!("State machine: Transitioned from Working to Waiting");

        self.steward.as_mut().unwrap().start_new_epoch().await;
        println!("State machine: Started new epoch");

        Ok(())
    }

    /// Start voting on proposals for the current epoch, transitioning to Voting state
    pub fn start_voting(&mut self) -> Result<(), GroupError> {
        println!(
            "State machine: start_voting called, current state: {:?}",
            self.state
        );
        if self.state != GroupState::Waiting {
            println!(
                "State machine: Invalid state transition from {:?} to Voting",
                self.state
            );
            return Err(GroupError::InvalidStateTransition);
        }

        self.state = GroupState::Voting;
        println!("State machine: Transitioned from Waiting to Voting");
        Ok(())
    }

    /// Complete voting and update state based on result
    pub fn complete_voting(&mut self, vote_result: bool) -> Result<(), GroupError> {
        println!(
            "State machine: complete_voting called with result {}, current state: {:?}",
            vote_result, self.state
        );
        if self.state != GroupState::Voting {
            println!(
                "State machine: Invalid state transition from {:?} to {}",
                self.state,
                if vote_result { "Waiting" } else { "Working" }
            );
            return Err(GroupError::InvalidStateTransition);
        }

        if vote_result {
            // Vote passed - stay in waiting state for proposal application
            self.state = GroupState::Waiting;
            println!("State machine: Vote passed, staying in Waiting state");
        } else {
            // Vote failed - return to working state
            self.state = GroupState::Working;
            println!("State machine: Vote failed, returning to Working state");
        }

        Ok(())
    }

    /// Apply proposals and complete the steward epoch
    pub async fn remove_proposals_and_complete(&mut self) -> Result<(), GroupError> {
        println!(
            "State machine: remove_proposals_and_complete called, current state: {:?}",
            self.state
        );
        if self.state != GroupState::Waiting {
            println!(
                "State machine: Invalid state transition from {:?} to Working",
                self.state
            );
            return Err(GroupError::InvalidStateTransition);
        }

        // Apply proposals for current epoch from steward
        if let Some(steward) = &mut self.steward {
            steward.empty_voting_epoch_proposals().await;
        } else {
            return Err(GroupError::StewardNotSet);
        }

        self.state = GroupState::Working;
        println!("State machine: Transitioned from Waiting to Working");

        Ok(())
    }

    /// Get the count of proposals in the current epoch
    pub async fn get_current_epoch_proposals_count(&self) -> usize {
        if let Some(steward) = &self.steward {
            steward.get_current_epoch_proposals_count().await
        } else {
            0
        }
    }

    /// Get the count of proposals in the voting epoch
    pub async fn get_voting_epoch_proposals_count(&self) -> usize {
        if let Some(steward) = &self.steward {
            steward.get_voting_epoch_proposals_count().await
        } else {
            0
        }
    }

    /// Get the proposals in the voting epoch
    pub async fn get_voting_epoch_proposals(&self) -> Vec<GroupUpdateRequest> {
        if let Some(steward) = &self.steward {
            steward.get_voting_epoch_proposals().await
        } else {
            Vec::new()
        }
    }

    /// Add a proposal to the current epoch
    pub async fn add_proposal(&mut self, proposal: GroupUpdateRequest) {
        if let Some(steward) = &mut self.steward {
            steward.add_proposal(proposal).await;
        }
    }

    /// Check if this state machine has a steward
    pub fn has_steward(&self) -> bool {
        self.steward.is_some()
    }

    /// Get a reference to the steward (if available)
    pub fn get_steward(&self) -> Option<&Steward> {
        self.steward.as_ref()
    }

    /// Get a mutable reference to the steward (if available)
    pub fn get_steward_mut(&mut self) -> Option<&mut Steward> {
        self.steward.as_mut()
    }
}

impl Default for GroupStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_state_machine_creation() {
        let state_machine = GroupStateMachine::new();
        assert_eq!(state_machine.current_state(), GroupState::Working);
        assert!(!state_machine.has_steward());
    }

    #[tokio::test]
    async fn test_state_machine_with_steward_creation() {
        let state_machine = GroupStateMachine::new_with_steward();
        assert_eq!(state_machine.current_state(), GroupState::Working);
        assert!(state_machine.has_steward());
    }

    #[tokio::test]
    async fn test_state_transitions() {
        let mut state_machine = GroupStateMachine::new_with_steward();

        // Initial state should be Working
        assert_eq!(state_machine.current_state(), GroupState::Working);

        // Test start_steward_epoch
        let proposals = state_machine
            .start_steward_epoch()
            .await
            .expect("Failed to start steward epoch");
        assert_eq!(state_machine.current_state(), GroupState::Waiting);

        // Test start_voting
        state_machine
            .start_voting()
            .expect("Failed to start voting");
        assert_eq!(state_machine.current_state(), GroupState::Voting);

        // Test complete_voting with success
        state_machine
            .complete_voting(true)
            .expect("Failed to complete voting");
        assert_eq!(state_machine.current_state(), GroupState::Waiting);

        // Test apply_proposals_and_complete
        let applied_proposals = state_machine
            .remove_proposals_and_complete()
            .await
            .expect("Failed to apply proposals");
        assert_eq!(state_machine.current_state(), GroupState::Working);
        assert_eq!(applied_proposals, proposals);
    }

    #[tokio::test]
    async fn test_message_permissions() {
        let mut state_machine = GroupStateMachine::new_with_steward();

        // Working state - anyone can send messages
        assert!(state_machine.can_send_message(false, false)); // Regular user, no proposals
        assert!(state_machine.can_send_message(true, false)); // Steward, no proposals
        assert!(state_machine.can_send_message(true, true)); // Steward, with proposals

        // Start steward epoch
        state_machine
            .start_steward_epoch()
            .await
            .expect("Failed to start steward epoch");

        // Waiting state - only steward with proposals can send messages
        assert!(!state_machine.can_send_message(false, false)); // Regular user, no proposals
        assert!(!state_machine.can_send_message(false, true)); // Regular user, with proposals
        assert!(!state_machine.can_send_message(true, false)); // Steward, no proposals
        assert!(state_machine.can_send_message(true, true)); // Steward, with proposals

        // Start voting
        state_machine
            .start_voting()
            .expect("Failed to start voting");

        // Voting state - no one can send messages
        assert!(!state_machine.can_send_message(false, false));
        assert!(!state_machine.can_send_message(false, true));
        assert!(!state_machine.can_send_message(true, false));
        assert!(!state_machine.can_send_message(true, true));
    }

    #[tokio::test]
    async fn test_invalid_state_transitions() {
        let mut state_machine = GroupStateMachine::new();

        // Cannot start voting from Working state
        let result = state_machine.start_voting();
        assert!(matches!(result, Err(GroupError::InvalidStateTransition)));

        // Cannot complete voting from Working state
        let result = state_machine.complete_voting(true);
        assert!(matches!(result, Err(GroupError::InvalidStateTransition)));

        // Cannot apply proposals from Working state
        let result = state_machine.remove_proposals_and_complete().await;
        assert!(matches!(result, Err(GroupError::InvalidStateTransition)));
    }

    #[tokio::test]
    async fn test_proposal_management() {
        let mut state_machine = GroupStateMachine::new_with_steward();

        // Add some proposals
        state_machine
            .add_proposal(GroupUpdateRequest::RemoveMember(vec![1, 2, 3]))
            .await;

        // Start steward epoch - should collect proposals
        state_machine
            .start_steward_epoch()
            .await
            .expect("Failed to start steward epoch");
        assert_eq!(state_machine.get_voting_epoch_proposals_count().await, 1);

        // Complete the flow
        state_machine
            .start_voting()
            .expect("Failed to start voting");
        state_machine
            .complete_voting(true)
            .expect("Failed to complete voting");
        state_machine
            .remove_proposals_and_complete()
            .await
            .expect("Failed to apply proposals");

        // Proposals should be applied and count should be reset
        assert_eq!(state_machine.get_current_epoch_proposals_count().await, 0);
    }
}
