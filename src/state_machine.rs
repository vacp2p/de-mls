//! State machine for steward epoch management and group operations.
//!
//! This module implements a state machine that manages the lifecycle of steward epochs,
//! proposal collection, voting, and application. The state machine ensures proper
//! transitions and enforces permissions at each state.
//!
//! # States
//!
//! - **Working**: Normal operation state where users can send any message freely
//! - **Waiting**: Steward epoch state where only steward can send BATCH_PROPOSALS_MESSAGE (if proposals exist)
//! - **Voting**: Voting state where everyone can send VOTE/USER_VOTE, only steward can send VOTING_PROPOSAL/PROPOSAL
//! - **ConsensusReached**: Consensus achieved, waiting for steward to send batch proposals
//! - **ConsensusFailed**: Consensus failed due to timeout or other reasons
//!
//! # State Transitions
//!
//! ```text
//! Working -- start_steward_epoch_with_validation() --> Waiting (if proposals exist)
//! Working -- start_steward_epoch_with_validation() --> Working (if no proposals, returns 0)
//! Waiting -- start_voting() --> Voting
//! Voting -- complete_voting(true) --> ConsensusReached (vote passed)
//! Voting -- complete_voting(false) --> Working (vote failed)
//! ConsensusReached -- start_waiting_after_consensus() --> Waiting (steward sends batch proposals)
//! Waiting -- handle_yes_vote() --> Working (after successful vote and proposal application)
//! ConsensusFailed -- recover_from_consensus_failure() --> Working (recovery)
//! ```
//!
//! # Message Type Permissions by State
//!
//! ## Working State
//! - **All users**: Can send any message type
//!
//! ## Waiting State
//! - **Steward with proposals**: Can send BATCH_PROPOSALS_MESSAGE
//! - **All users**: All other message types blocked
//!
//! ## Voting State
//! - **All users**: Can send VOTE and USER_VOTE
//! - **Steward only**: Can send VOTING_PROPOSAL and PROPOSAL
//! - **All users**: All other message types blocked
//!
//! ## ConsensusReached State
//! - **Steward with proposals**: Can send BATCH_PROPOSALS_MESSAGE
//! - **All users**: All other message types blocked
//!
//! ## ConsensusFailed State
//! - **All users**: No messages allowed
//!
//! # Steward Flow Scenarios
//!
//! ## Scenario 1: No Proposals Initially
//! ```text
//! Working --start_steward_epoch_with_validation()--> Working (stays in Working, returns 0)
//! ```
//!
//! ## Scenario 2: Successful Vote with Proposals
//! **Steward:**
//! ```text
//! Working --start_steward_epoch_with_validation()--> Waiting --start_voting()--> Voting
//!         --complete_voting(true)--> ConsensusReached --start_waiting_after_consensus()--> Waiting
//!         --handle_yes_vote()--> Working
//! ```
//! **Non-Steward:**
//! ```text
//! Working --steward_starts_epoch()--> Waiting --start_voting()--> Voting
//!         --start_consensus_reached()--> ConsensusReached --start_waiting()--> Waiting
//!         --handle_yes_vote()--> Working
//! ```
//!
//! ## Scenario 3: Failed Vote
//! **Steward:**
//! ```text
//! Working --start_steward_epoch_with_validation()--> Waiting --start_voting()--> Voting
//!         --complete_voting(false)--> Working
//! ```
//! **Non-Steward:**
//! ```text
//! Working --steward_starts_epoch()--> Waiting --start_voting()--> Voting
//!         --start_consensus_reached()--> ConsensusReached --start_consensus_failed()--> ConsensusFailed
//!         --recover_from_consensus_failure()--> Working
//! ```
//!
//! # Key Methods
//!
//! - `start_steward_epoch_with_validation()`: Main entry point for starting steward epochs with proposal validation
//! - `start_voting()`: Transitions to voting state from any non-voting state
//! - `complete_voting(vote_result)`: Handles voting completion and transitions based on result
//! - `handle_yes_vote()`: Applies proposals and returns to working state after successful vote
//! - `start_waiting_after_consensus()`: Transitions from ConsensusReached to Waiting for batch proposal processing
//! - `recover_from_consensus_failure()`: Recovers from consensus failure back to Working state
//!
//! # Proposal Management
//!
//! - Proposals are collected in the current epoch and moved to voting epoch when steward epoch starts
//! - After successful voting, proposals are applied and cleared from voting epoch
//! - Failed votes result in proposals being discarded and return to working state

use std::fmt::Display;

use log::info;

use crate::message::message_types;
use crate::steward::Steward;
use crate::{steward::GroupUpdateRequest, GroupError};

/// Represents the different states a group can be in during the steward epoch flow
#[derive(Debug, Clone, PartialEq)]
pub enum GroupState {
    /// Normal operation state - users can send any message freely
    Working,
    /// Waiting state during steward epoch - only steward can send BATCH_PROPOSALS_MESSAGE
    Waiting,
    /// Voting state - everyone can send VOTE/USER_VOTE, only steward can send VOTING_PROPOSAL/PROPOSAL
    Voting,
    /// Consensus reached state - consensus achieved, waiting for steward to send batch proposals
    ConsensusReached,
    /// Consensus failed state - consensus failed due to timeout or other reasons
    ConsensusFailed,
}

impl Display for GroupState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self {
            GroupState::Working => "Working",
            GroupState::Waiting => "Waiting",
            GroupState::Voting => "Voting",
            GroupState::ConsensusReached => "ConsensusReached",
            GroupState::ConsensusFailed => "ConsensusFailed",
        };
        write!(f, "{state}")
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

    /// Check if a specific message type can be sent in the current state.
    ///
    /// ## Parameters:
    /// - `is_steward`: Whether the sender is a steward
    /// - `has_proposals`: Whether there are proposals available (for steward operations)
    /// - `message_type`: The type of message to check
    ///
    /// ## Returns:
    /// - `true` if the message can be sent, `false` otherwise
    ///
    /// ## Usage:
    /// Used to enforce message type permissions based on current state and sender role.
    /// This ensures proper state machine behavior and prevents invalid operations.
    pub fn can_send_message_type(
        &self,
        is_steward: bool,
        has_proposals: bool,
        message_type: &str,
    ) -> bool {
        match self.state {
            GroupState::Working => true, // Anyone can send any message in working state
            GroupState::Waiting => {
                // In waiting state, only steward can send BATCH_PROPOSALS_MESSAGE
                match message_type {
                    message_types::BATCH_PROPOSALS_MESSAGE => is_steward && has_proposals,
                    _ => false, // All other messages blocked during waiting
                }
            }
            GroupState::Voting => {
                // In voting state, only voting-related messages allowed
                match message_type {
                    message_types::VOTE => true,                  // Everyone can send votes
                    message_types::USER_VOTE => true,             // Everyone can send user votes
                    message_types::VOTING_PROPOSAL => is_steward, // Only steward can send voting proposals
                    message_types::PROPOSAL => is_steward,        // Only steward can send proposals
                    _ => false, // All other messages blocked during voting
                }
            }
            GroupState::ConsensusReached => {
                // In ConsensusReached state, only steward can send BATCH_PROPOSALS_MESSAGE
                match message_type {
                    message_types::BATCH_PROPOSALS_MESSAGE => is_steward && has_proposals,
                    _ => false, // All other messages blocked during ConsensusReached
                }
            }
            GroupState::ConsensusFailed => {
                // In ConsensusFailed state, no messages are allowed
                false
            }
        }
    }

    /// Start voting on proposals for the current epoch, transitioning to Voting state.
    ///
    /// ## Preconditions:
    /// - Can be called from any state except Voting (prevents double voting)
    ///
    /// ## State Transition:
    /// Any State (except Voting) → Voting
    pub fn start_voting(&mut self) -> Result<(), GroupError> {
        if self.state == GroupState::Voting {
            return Err(GroupError::InvalidStateTransition {
                from: self.state.to_string(),
                to: "Voting".to_string(),
            });
        }
        self.state = GroupState::Voting;
        Ok(())
    }

    /// Complete voting and update state based on result.
    ///
    /// ## Preconditions:
    /// - Must be in Voting state
    ///
    /// ## State Transitions:
    /// - Vote YES: Voting → ConsensusReached (consensus achieved, waiting for batch proposals)
    /// - Vote NO: Voting → Working (proposals discarded)
    pub fn complete_voting(&mut self, vote_result: bool) -> Result<(), GroupError> {
        if self.state != GroupState::Voting {
            return Err(GroupError::InvalidStateTransition {
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
            // Vote YES - go to ConsensusReached state to wait for steward to send batch proposals
            info!("[complete_voting]: Vote YES, transitioning to ConsensusReached state");
            self.start_consensus_reached();
        } else {
            // Vote NO - return to working state
            info!("[complete_voting]: Vote NO, transitioning to Working state");
            self.start_working();
        }

        Ok(())
    }

    /// Start consensus reached state (for non-steward peers after consensus).
    ///
    /// ## State Transition:
    /// Any State → ConsensusReached
    ///
    /// ## Usage:
    /// Called by non-steward peers when consensus is reached during voting.
    /// This allows them to transition to the appropriate state for waiting
    /// for the steward to process and send batch proposals.
    pub fn start_consensus_reached(&mut self) {
        self.state = GroupState::ConsensusReached;
        info!("[start_consensus_reached] Transitioning to ConsensusReached state");
    }

    /// Start consensus failed state (for peers after consensus failure).
    ///
    /// ## State Transition:
    /// Any State → ConsensusFailed
    ///
    /// ## Usage:
    /// Called when consensus fails due to timeout or other reasons.
    /// This state blocks all message types until recovery is initiated.
    pub fn start_consensus_failed(&mut self) {
        self.state = GroupState::ConsensusFailed;
        info!("[start_consensus_failed] Transitioning to ConsensusFailed state");
    }

    /// Recover from consensus failure by transitioning back to Working state
    pub fn recover_from_consensus_failure(&mut self) -> Result<(), GroupError> {
        if self.state != GroupState::ConsensusFailed {
            return Err(GroupError::InvalidStateTransition {
                from: self.state.to_string(),
                to: "Working".to_string(),
            });
        }

        self.state = GroupState::Working;
        info!("[recover_from_consensus_failure] Recovering from consensus failure, transitioning to Working state");
        Ok(())
    }

    /// Start working state (for non-steward peers after consensus or edge case recovery).
    ///
    /// ## State Transition:
    /// Any State → Working
    ///
    /// ## Usage:
    /// - Non-steward peers: Called after receiving consensus results
    /// - Edge case recovery: Called when proposals disappear during voting phase
    /// - General recovery: Can be used to reset to normal operation from any state
    ///
    /// ## Note:
    /// This method provides a safe way to transition back to normal operation
    /// and is commonly used for recovery scenarios.
    pub fn start_working(&mut self) {
        self.state = GroupState::Working;
        info!("[start_working] Transitioning to Working state");
    }

    /// Start waiting state (for non-steward peers after consensus).
    ///
    /// ## State Transition:
    /// Any State → Waiting
    ///
    /// ## Usage:
    /// Called by non-steward peers to transition to waiting state,
    /// typically after consensus is reached and they need to wait for
    /// the steward to process and send batch proposals.
    pub fn start_waiting(&mut self) {
        self.state = GroupState::Waiting;
        info!("[start_waiting] Transitioning to Waiting state");
    }

    /// Get the count of proposals in the current epoch.
    ///
    /// ## Returns:
    /// - Number of proposals currently collected for the next steward epoch
    ///
    /// ## Usage:
    /// Used to check if there are proposals to vote on before starting a steward epoch.
    pub async fn get_current_epoch_proposals_count(&self) -> usize {
        if let Some(steward) = &self.steward {
            steward.get_current_epoch_proposals_count().await
        } else {
            0
        }
    }

    /// Get the count of proposals in the voting epoch.
    ///
    /// ## Returns:
    /// - Number of proposals currently being voted on
    ///
    /// ## Usage:
    /// Used during voting to track how many proposals are being considered.
    pub async fn get_voting_epoch_proposals_count(&self) -> usize {
        if let Some(steward) = &self.steward {
            steward.get_voting_epoch_proposals_count().await
        } else {
            0
        }
    }

    /// Get the proposals in the voting epoch.
    ///
    /// ## Returns:
    /// - Vector of proposals currently being voted on
    ///
    /// ## Usage:
    /// Used during voting to access the actual proposal details for processing.
    pub async fn get_voting_epoch_proposals(&self) -> Vec<GroupUpdateRequest> {
        if let Some(steward) = &self.steward {
            steward.get_voting_epoch_proposals().await
        } else {
            Vec::new()
        }
    }

    /// Add a proposal to the current epoch.
    ///
    /// ## Parameters:
    /// - `proposal`: The group update request to add
    ///
    /// ## Usage:
    /// Called to submit new proposals for consideration in the next steward epoch.
    /// Proposals are collected and will be moved to the voting epoch when
    /// `start_steward_epoch_with_validation()` is called.
    pub async fn add_proposal(&mut self, proposal: GroupUpdateRequest) {
        if let Some(steward) = &mut self.steward {
            steward.add_proposal(proposal).await;
        }
    }

    /// Check if this state machine has a steward configured.
    ///
    /// ## Returns:
    /// - `true` if a steward is configured, `false` otherwise
    ///
    /// ## Usage:
    /// Used to verify steward availability before attempting steward epoch operations.
    pub fn has_steward(&self) -> bool {
        self.steward.is_some()
    }

    /// Get a reference to the steward (if available).
    ///
    /// ## Returns:
    /// - `Some(&Steward)` if steward is configured, `None` otherwise
    ///
    /// ## Usage:
    /// Used to access steward functionality for read-only operations.
    pub fn get_steward(&self) -> Option<&Steward> {
        self.steward.as_ref()
    }

    /// Get a mutable reference to the steward (if available).
    ///
    /// ## Returns:
    /// - `Some(&mut Steward)` if steward is configured, `None` otherwise
    ///
    /// ## Usage:
    /// Used to access steward functionality for read-write operations.
    pub fn get_steward_mut(&mut self) -> Option<&mut Steward> {
        self.steward.as_mut()
    }

    /// Handle steward epoch start with proposal validation.
    /// This is the main entry point for starting steward epochs.
    ///
    /// ## Preconditions:
    /// - Must be in Working state
    /// - Must have a steward configured
    ///
    /// ## State Transitions:
    /// - **With proposals**: Working → Waiting (returns proposal count)
    /// - **No proposals**: Working → Working (stays in Working, returns 0)
    ///
    /// ## Returns:
    /// - Number of proposals collected for voting (0 if no proposals)
    ///
    /// ## Usage:
    /// This method should be used instead of `start_steward_epoch()` for external calls
    /// as it provides proper proposal validation and state management.
    pub async fn start_steward_epoch_with_validation(&mut self) -> Result<usize, GroupError> {
        if self.state != GroupState::Working {
            return Err(GroupError::InvalidStateTransition {
                from: self.state.to_string(),
                to: "Waiting".to_string(),
            });
        }

        // Always check if steward is set - required for steward epoch operations
        if !self.has_steward() {
            return Err(GroupError::StewardNotSet);
        }

        // Check if there are proposals to vote on
        let proposal_count = self.get_current_epoch_proposals_count().await;

        if proposal_count == 0 {
            // No proposals, stay in Working state but still return 0
            // This indicates a successful steward epoch start with no proposals
            Ok(0)
        } else {
            // Start steward epoch and transition to Waiting
            self.state = GroupState::Waiting;
            self.steward
                .as_mut()
                .ok_or(GroupError::StewardNotSet)?
                .start_new_epoch()
                .await;
            Ok(proposal_count)
        }
    }

    /// Handle proposal application and completion after successful voting.
    ///
    /// ## Preconditions:
    /// - Must be in ConsensusReached or Waiting state
    /// - Must have a steward configured
    ///
    /// ## State Transition:
    /// ConsensusReached/Waiting → Working
    ///
    /// ## Actions:
    /// - Clears voting epoch proposals
    /// - Transitions to Working state
    ///
    /// ## Usage:
    /// Called after successful voting to empty the voting epoch proposals and transition to Working state.
    pub async fn handle_yes_vote(&mut self) -> Result<(), GroupError> {
        // Check state transition validity - can be called from ConsensusReached or Waiting state
        if self.state != GroupState::ConsensusReached && self.state != GroupState::Waiting {
            return Err(GroupError::InvalidStateTransition {
                from: self.state.to_string(),
                to: "Working".to_string(),
            });
        }

        let steward = self.steward.as_mut().ok_or(GroupError::StewardNotSet)?;
        steward.empty_voting_epoch_proposals().await;

        self.state = GroupState::Working;

        Ok(())
    }

    /// Start waiting state when steward sends batch proposals after consensus.
    /// This transitions from ConsensusReached to Waiting state.
    ///
    /// ## Preconditions:
    /// - Must be in ConsensusReached state
    ///
    /// ## State Transition:
    /// ConsensusReached → Waiting
    ///
    /// ## Usage:
    /// Called when steward needs to send batch proposals after consensus is reached.
    /// This allows the steward to process and send proposals while maintaining proper state flow.
    pub fn start_waiting_after_consensus(&mut self) -> Result<(), GroupError> {
        if self.state != GroupState::ConsensusReached {
            return Err(GroupError::InvalidStateTransition {
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

    /// Handle failed vote cleanup.
    ///
    /// ## Preconditions:
    /// - Must have a steward configured
    ///
    /// ## Actions:
    /// - Clears voting epoch proposals
    /// - Does not change state
    ///
    /// ## Usage:
    /// Called after failed votes to clean up proposals. The caller is responsible
    /// for transitioning to the appropriate state (typically Working).
    pub async fn handle_no_vote(&mut self) -> Result<(), GroupError> {
        let steward = self.steward.as_mut().ok_or(GroupError::StewardNotSet)?;
        steward.empty_voting_epoch_proposals().await;
        Ok(())
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
        state_machine
            .start_steward_epoch_with_validation()
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
        assert_eq!(state_machine.current_state(), GroupState::ConsensusReached);

        // Test start_waiting_after_consensus
        state_machine
            .start_waiting_after_consensus()
            .expect("Failed to start waiting after consensus");
        assert_eq!(state_machine.current_state(), GroupState::Waiting);

        // Test apply_proposals_and_complete
        state_machine
            .handle_yes_vote()
            .await
            .expect("Failed to apply proposals");
        assert_eq!(state_machine.current_state(), GroupState::Working);
    }

    #[tokio::test]
    async fn test_message_type_permissions() {
        let mut state_machine = GroupStateMachine::new_with_steward();

        // Working state - all message types allowed
        assert!(state_machine.can_send_message_type(false, false, message_types::BAN_REQUEST));
        assert!(state_machine.can_send_message_type(
            false,
            false,
            message_types::CONVERSATION_MESSAGE
        ));
        assert!(state_machine.can_send_message_type(
            true,
            false,
            message_types::BATCH_PROPOSALS_MESSAGE
        ));

        // Start steward epoch
        state_machine
            .start_steward_epoch_with_validation()
            .await
            .expect("Failed to start steward epoch");

        // Waiting state - test specific message types
        // All messages allowed from anyone EXCEPT BATCH_PROPOSALS_MESSAGE
        assert!(!state_machine.can_send_message_type(false, false, message_types::BAN_REQUEST));
        assert!(!state_machine.can_send_message_type(
            false,
            false,
            message_types::CONVERSATION_MESSAGE
        ));
        assert!(!state_machine.can_send_message_type(false, false, message_types::VOTE));
        assert!(!state_machine.can_send_message_type(false, false, message_types::USER_VOTE));
        assert!(!state_machine.can_send_message_type(false, false, message_types::VOTING_PROPOSAL));
        assert!(!state_machine.can_send_message_type(false, false, message_types::PROPOSAL));

        // BatchProposalsMessage should only be allowed from steward with proposals
        assert!(!state_machine.can_send_message_type(
            false,
            false,
            message_types::BATCH_PROPOSALS_MESSAGE
        ));
        assert!(!state_machine.can_send_message_type(
            true,
            false,
            message_types::BATCH_PROPOSALS_MESSAGE
        ));
        assert!(state_machine.can_send_message_type(
            true,
            true,
            message_types::BATCH_PROPOSALS_MESSAGE
        ));

        // Start voting
        state_machine
            .start_voting()
            .expect("Failed to start voting");

        // Voting state - only voting-related messages allowed
        // Everyone can send votes and user votes
        assert!(state_machine.can_send_message_type(false, false, message_types::VOTE));
        assert!(state_machine.can_send_message_type(false, false, message_types::USER_VOTE));

        // Only steward can send voting proposals and proposals
        assert!(!state_machine.can_send_message_type(false, false, message_types::VOTING_PROPOSAL));
        assert!(state_machine.can_send_message_type(true, false, message_types::VOTING_PROPOSAL));
        assert!(!state_machine.can_send_message_type(false, false, message_types::PROPOSAL));
        assert!(state_machine.can_send_message_type(true, false, message_types::PROPOSAL));

        // All other message types blocked during voting
        assert!(!state_machine.can_send_message_type(
            false,
            false,
            message_types::CONVERSATION_MESSAGE
        ));
        assert!(!state_machine.can_send_message_type(false, false, message_types::BAN_REQUEST));
        assert!(!state_machine.can_send_message_type(
            false,
            false,
            message_types::BATCH_PROPOSALS_MESSAGE
        ));
    }

    #[tokio::test]
    async fn test_invalid_state_transitions() {
        let mut state_machine = GroupStateMachine::new();

        // Cannot complete voting from Working state
        let result = state_machine.complete_voting(true);
        assert!(matches!(
            result,
            Err(GroupError::InvalidStateTransition { .. })
        ));

        // Cannot apply proposals from Working state
        let result = state_machine.handle_yes_vote().await;
        assert!(matches!(
            result,
            Err(GroupError::InvalidStateTransition { .. })
        ));
    }

    #[tokio::test]
    async fn test_proposal_management() {
        let mut state_machine = GroupStateMachine::new_with_steward();

        // Add some proposals
        state_machine
            .add_proposal(GroupUpdateRequest::RemoveMember(
                "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc".to_string(),
            ))
            .await;

        // Start steward epoch - should collect proposals
        state_machine
            .start_steward_epoch_with_validation()
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
            .handle_yes_vote()
            .await
            .expect("Failed to apply proposals");

        // Proposals should be applied and count should be reset
        assert_eq!(state_machine.get_current_epoch_proposals_count().await, 0);
    }

    #[tokio::test]
    async fn test_state_snapshot_consistency() {
        let mut state_machine = GroupStateMachine::new_with_steward();

        // Add some proposals
        state_machine
            .add_proposal(GroupUpdateRequest::RemoveMember(
                "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc".to_string(),
            ))
            .await;

        // Get a snapshot before state transition
        let snapshot1 = state_machine.get_current_epoch_proposals_count().await;
        assert_eq!(snapshot1, 1);

        // Start steward epoch
        state_machine
            .start_steward_epoch_with_validation()
            .await
            .expect("Failed to start steward epoch");

        // Get a snapshot after state transition
        let snapshot2 = state_machine.get_current_epoch_proposals_count().await;
        assert_eq!(snapshot2, 0);

        // Verify that the snapshots are consistent within themselves
        assert!(snapshot1 > 0);
        assert_ne!(snapshot1, snapshot2);
    }
}
