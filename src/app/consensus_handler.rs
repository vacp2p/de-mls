//! Consensus event handling and orchestration.
//!
//! This module provides utilities for handling consensus events
//! and orchestrating the steward voting flow.

use ds::transport::OutboundPacket;
use hashgraph_like_consensus::types::ConsensusEvent;
use tracing::info;

use super::state_machine::{GroupState, GroupStateMachine, StateMachineError};
use crate::core::{self, CoreError, GroupHandle};
use mls_crypto::MlsGroupService;

/// Handler for consensus events.
pub struct ConsensusHandler;

impl ConsensusHandler {
    /// Handle a consensus event for a group.
    ///
    /// # Arguments
    /// * `handle` - The group handle
    /// * `state_machine` - The group's state machine
    /// * `event` - The consensus event
    /// * `mls` - MLS service for batch proposal creation
    ///
    /// # Returns
    /// A vector of outbound packets to send, if any.
    pub async fn handle_event(
        handle: &mut GroupHandle,
        state_machine: &mut GroupStateMachine,
        event: ConsensusEvent,
        mls: &dyn MlsGroupService,
    ) -> Result<Vec<OutboundPacket>, ConsensusHandlerError> {
        match event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
                timestamp: _,
            } => {
                info!("Consensus reached for proposal {proposal_id}: result={result}");
                Self::handle_consensus_result(handle, state_machine, result, mls).await
            }
            ConsensusEvent::ConsensusFailed {
                proposal_id,
                timestamp: _,
            } => {
                info!("Consensus failed for proposal {proposal_id}");
                if state_machine.current_state() == GroupState::Voting {
                    Self::handle_consensus_result(handle, state_machine, false, mls).await
                } else {
                    Err(ConsensusHandlerError::InvalidState(
                        state_machine.current_state().to_string(),
                    ))
                }
            }
        }
    }

    /// Handle the result of consensus.
    ///
    /// # Arguments
    /// * `handle` - The group handle
    /// * `state_machine` - The group's state machine
    /// * `vote_result` - `true` for YES, `false` for NO
    /// * `mls` - MLS service for batch proposal creation
    async fn handle_consensus_result(
        handle: &mut GroupHandle,
        state_machine: &mut GroupStateMachine,
        vote_result: bool,
        mls: &dyn MlsGroupService,
    ) -> Result<Vec<OutboundPacket>, ConsensusHandlerError> {
        // Update state machine
        state_machine.complete_voting(vote_result)?;

        let is_steward = state_machine.is_steward();

        if is_steward && vote_result {
            info!("[handle_consensus_result]: Steward got YES → applying proposals");

            // Create and send batch proposals
            let messages = core::create_batch_proposals(handle, mls).await?;

            // Complete the voting
            core::complete_voting(handle, true);
            state_machine.handle_yes_vote()?;

            Ok(messages)
        } else if is_steward && !vote_result {
            info!("[handle_consensus_result]: Steward got NO → discarding proposals");
            core::complete_voting(handle, false);
            state_machine.start_working();
            Ok(vec![])
        } else if !is_steward && vote_result {
            info!("[handle_consensus_result]: Member got YES → Waiting for batch");
            state_machine.start_waiting_after_consensus()?;
            Ok(vec![])
        } else {
            info!("[handle_consensus_result]: Member got NO → Working");
            state_machine.start_working();
            Ok(vec![])
        }
    }
}

/// Errors from consensus handling.
#[derive(Debug, thiserror::Error)]
pub enum ConsensusHandlerError {
    /// State machine error.
    #[error("State machine error: {0}")]
    StateMachine(#[from] StateMachineError),

    /// Core library error.
    #[error("Core error: {0}")]
    Core(#[from] CoreError),

    /// Invalid state for operation.
    #[error("Invalid state: {0}")]
    InvalidState(String),
}
