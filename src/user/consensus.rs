use ds::OutboundPacket;
use hashgraph_like_consensus::types::ConsensusEvent;
use tracing::{error, info};

use crate::{
    error::UserError,
    state_machine::GroupState,
    user::{User, UserAction},
};

impl User {
    /// Application-level handler for a finalized consensus outcome.
    ///
    /// The `hashgraph-like-consensus` library determines the outcome and emits events.
    /// This function is responsible only for:
    /// - driving the group state machine (Voting → ConsensusReached/Working/etc.)
    /// - creating any MLS commit / batch proposal messages (steward YES path)
    /// - replaying any stored batch proposals (non-steward YES path)
    async fn handle_consensus_result(
        &mut self,
        group_name: &str,
        vote_result: bool,
    ) -> Result<Vec<OutboundPacket>, UserError> {
        let group = self.group_ref(group_name).await?;

        // Voting -> (ConsensusReached | Working) depending on result
        group.write().await.complete_voting(vote_result).await?;

        let is_steward = group.read().await.is_steward().await;
        if is_steward {
            if vote_result {
                info!("[handle_consensus_result]: Steward YES → applying proposals + emitting commit/batch msgs");
                let messages = self.apply_proposals(group_name).await?;
                group.write().await.handle_yes_vote().await?;
                Ok(messages)
            } else {
                info!("[handle_consensus_result]: Steward NO → discarding proposals");
                group.write().await.handle_no_vote().await?;
                group.write().await.start_working().await;
                Ok(vec![])
            }
        } else if vote_result {
            // Non-steward: wait for steward's batch proposals message.
            group.write().await.start_waiting_after_consensus().await?;
            info!("[handle_consensus_result]: Member YES → Waiting (may replay pending batch proposals)");

            if self.pending_batch_proposals.contains(group_name).await {
                let action = self.process_stored_batch_proposals(group_name).await?;
                if let Some(action) = action {
                    match action {
                        UserAction::Outbound(outbound_packet) => Ok(vec![outbound_packet]),
                        UserAction::LeaveGroup(group_name) => {
                            self.leave_group(group_name.as_str()).await?;
                            Ok(vec![])
                        }
                        UserAction::DoNothing => Ok(vec![]),
                        other => {
                            error!("[handle_consensus_result]: Unexpected action after replaying pending batches: {other}");
                            Err(UserError::InvalidUserAction(other.to_string()))
                        }
                    }
                } else {
                    Ok(vec![])
                }
            } else {
                Ok(vec![])
            }
        } else {
            // Non-steward: vote failed, return to Working.
            group.write().await.start_working().await;
            info!("[handle_consensus_result]: Member NO → Working");
            Ok(vec![])
        }
    }

    /// Handle incoming consensus events and return commit messages if needed.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group the consensus event is for
    /// - `event`: The consensus event to handle
    ///
    /// ## Returns:
    /// - Vector of Waku messages to send (if any)
    ///
    /// ## Event Types Handled:
    /// - **ConsensusReached**: Handles successful consensus with result
    /// - **ConsensusFailed**: Handles consensus failure with liveness criteria
    ///
    /// ## Effects:
    /// - Routes consensus events to appropriate handlers
    /// - Manages state transitions based on consensus results
    /// - Applies liveness criteria for failed consensus
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::InvalidGroupState` if group is in invalid state
    /// - Various consensus handling errors
    pub async fn handle_consensus_event(
        &mut self,
        group_name: &str,
        event: ConsensusEvent,
    ) -> Result<Vec<OutboundPacket>, UserError> {
        match event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
                timestamp: _,
            } => {
                info!(
                        "[handle_consensus_event]: Consensus reached for proposal {proposal_id} in group {group_name}: {result}"
                    );

                let group = self.group_ref(group_name).await?;

                let current_state = group.read().await.get_state().await;
                info!(
                    "[handle_consensus_event]: Current state: {:?} for proposal {proposal_id}",
                    current_state
                );

                // Handle the consensus result and return commit messages
                let messages = self.handle_consensus_result(group_name, result).await?;
                Ok(messages)
            }
            ConsensusEvent::ConsensusFailed {
                proposal_id,
                timestamp: _,
            } => {
                info!(
                        "[handle_consensus_event]: Consensus failed for proposal {proposal_id} in group {group_name}"
                    );

                let group = self.group_ref(group_name).await?;

                let current_state = group.read().await.get_state().await;

                info!("[handle_consensus_event]: Handling consensus failure in {:?} state for proposal {proposal_id}", current_state);

                // The library emits ConsensusFailed when insufficient votes were collected by timeout.
                // App policy: treat this as a failed vote (NO) if we are still in Voting.
                if current_state == GroupState::Voting {
                    self.handle_consensus_result(group_name, false).await
                } else {
                    Err(UserError::InvalidGroupState(current_state.to_string()))
                }
            }
        }
    }

    /// Process user vote from frontend.
    ///
    /// ## Parameters:
    /// - `proposal_id`: The ID of the proposal to vote on
    /// - `user_vote`: The user's vote (true for yes, false for no)
    /// - `group_name`: The name of the group the vote is for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - For stewards: Creates consensus vote and sends to group
    /// - For regular users: Processes user vote in consensus service
    /// - Builds and sends appropriate message to group
    ///
    /// ## Message Types:
    /// - **Steward**: Sends consensus vote message
    /// - **Regular User**: Sends user vote message
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various consensus service and message building errors
    pub async fn process_user_vote(
        &mut self,
        proposal_id: u32,
        user_vote: bool,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        let group = self.group_ref(group_name).await?;
        let app_message = if group.read().await.is_steward().await {
            info!(
                    "[process_user_vote]: Steward voting for proposal {proposal_id} in group {group_name}"
                );
            let proposal = self
                .consensus_service
                .cast_vote_and_get_proposal(
                    &group_name.to_string(),
                    proposal_id,
                    user_vote,
                    self.eth_signer.clone(),
                )
                .await?;
            proposal.into()
        } else {
            info!(
                "[process_user_vote]: User voting for proposal {proposal_id} in group {group_name}"
            );
            let vote = self
                .consensus_service
                .cast_vote(
                    &group_name.to_string(),
                    proposal_id,
                    user_vote,
                    self.eth_signer.clone(),
                )
                .await?;
            vote.into()
        };

        self.build_group_message(app_message, group_name)
            .await
            .map(UserAction::Outbound)
    }
}
