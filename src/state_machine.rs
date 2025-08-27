//! State machine for steward epoch management and group operations.
//!
//! This module implements a state machine that manages the lifecycle of steward epochs,
//! proposal collection, voting, and application. The state machine ensures proper
//! transitions and enforces permissions at each state.
//!
//! # States
//!
//! - **Working**: Normal operation state where users can send any message freely
//! - **Waiting**: Steward epoch state where only steward can send BATCH_PROPOSALS_MESSAGE
//! - **Voting**: Voting state where everyone can send VOTE/USER_VOTE, only steward can send VOTING_PROPOSAL/PROPOSAL
//! - **ConsensusReached**: Consensus achieved, waiting for steward to send batch proposals
//! - **ConsensusFailed**: Consensus failed due to timeout or other reasons
//!
//! # State Transitions
//!
//! ```text
//! Working -- start_steward_epoch() --> Waiting (if proposals exist)
//! Working -- start_steward_epoch() --> Working (if no proposals)
//! Waiting -- start_voting() --> Voting
//! Voting -- complete_voting(true) --> Waiting (vote passed)
//! Voting -- complete_voting(false) --> Working (vote failed)
//! Waiting -- apply_proposals_and_complete() --> Working (after successful vote)
//! ```
//!
//! # Steward Flow Scenarios
//!
//! ## Scenario 1: No Proposals Initially
//! ```text
//! Working --start_steward_epoch()--> Working (stays in Working, no state change)
//! ```
//!
//! ## Scenario 2: Successful Vote
//! **Steward:**
//! ```text
//! Working --start_steward_epoch()--> Waiting --start_voting()--> Voting
//!         --complete_voting(true)--> Waiting --apply_proposals()--> Working
//! ```
//! **Non-Steward:**
//! ```text
//! Working --steward_starts_epoch()--> Waiting --start_voting()--> Voting
//!         --consensus_result(true)--> Waiting --apply_proposals()--> Working
//! ```
//!
//! ## Scenario 3: Failed Vote
//! **Steward:**
//! ```text
//! Working --start_steward_epoch()--> Waiting --start_voting()--> Voting
//!         --complete_voting(false)--> Working
//! ```
//! **Non-Steward:**
//! ```text
//! Working --steward_starts_epoch()--> Waiting --start_voting()--> Voting
//!         --consensus_result(false)--> Working
//! ```
//!

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

    /// Check if a specific message type can be sent in the current state
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

    /// Start a new steward epoch, transitioning to Waiting state.
    ///
    /// ## Preconditions:
    /// - Must be in Working state
    /// - Must have a steward configured
    ///
    /// ## State Transition:
    /// Working → Waiting
    pub async fn start_steward_epoch(&mut self) -> Result<(), GroupError> {
        if self.state != GroupState::Working {
            return Err(GroupError::InvalidStateTransition {
                from: self.state.to_string(),
                to: "Waiting".to_string(),
            });
        }
        self.state = GroupState::Waiting;
        self.steward
            .as_mut()
            .ok_or(GroupError::StewardNotSet)?
            .start_new_epoch()
            .await;
        Ok(())
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

    /// Start consensus reached state (for non-steward peers after consensus)
    pub fn start_consensus_reached(&mut self) {
        self.state = GroupState::ConsensusReached;
        info!("[start_consensus_reached] Transitioning to ConsensusReached state");
    }

    /// Start consensus failed state (for peers after consensus failure)
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
    /// ## Usage:
    /// - Non-steward peers: Called after receiving consensus results
    /// - Edge case recovery: Called when proposals disappear during voting phase
    ///
    /// ## State Transition:
    /// Any State → Working
    pub fn start_working(&mut self) {
        self.state = GroupState::Working;
        info!("[start_working] Transitioning to Working state");
    }

    /// Start waiting state (for non-steward peers after consensus)
    pub fn start_waiting(&mut self) {
        self.state = GroupState::Waiting;
        info!("[start_waiting] Transitioning to Waiting state");
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

    /// Handle steward epoch start with proposal validation
    /// This centralizes the logic for starting steward epochs
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
            self.start_steward_epoch().await?;
            Ok(proposal_count)
        }
    }

    /// Handle proposal application and completion
    /// This centralizes the logic for applying proposals after successful voting
    pub async fn handle_yes_vote(&mut self) -> Result<(), GroupError> {
        // Check state transition validity - can be called from ConsensusReached or Waiting state
        if self.state != GroupState::ConsensusReached && self.state != GroupState::Waiting {
            return Err(GroupError::InvalidStateTransition {
                from: self.state.to_string(),
                to: "Working".to_string(),
            });
        }

        if let Some(steward) = &mut self.steward {
            steward.empty_voting_epoch_proposals().await;
        } else {
            return Err(GroupError::StewardNotSet);
        }

        self.state = GroupState::Working;

        Ok(())
    }

    /// Start waiting state when steward sends batch proposals after consensus
    /// This transitions from ConsensusReached to Waiting state
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

    /// Handle failed vote cleanup
    /// This centralizes the logic for cleaning up after failed votes
    pub async fn handle_no_vote(&mut self) -> Result<(), GroupError> {
        if let Some(steward) = &mut self.steward {
            steward.empty_voting_epoch_proposals().await;
            Ok(())
        } else {
            Err(GroupError::StewardNotSet)
        }
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
            .start_steward_epoch()
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
            .start_steward_epoch()
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
