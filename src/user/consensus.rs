use tracing::{error, info};

use crate::{
    consensus::ConsensusEvent,
    error::UserError,
    protos::{
        consensus::v1::{Proposal, Vote, VotePayload},
        de_mls::messages::v1::AppMessage,
    },
    state_machine::GroupState,
    user::{User, UserAction},
};
use ds::net::OutboundPacket;

impl User {
    pub async fn set_up_consensus_threshold_for_group(
        &mut self,
        group_name: &str,
        proposal_id: u32,
        consensus_threshold: f64,
    ) -> Result<(), UserError> {
        self.consensus_service
            .set_consensus_threshold_for_group_session(group_name, proposal_id, consensus_threshold)
            .await?;
        Ok(())
    }
    /// Handle consensus result after it's determined.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group the consensus is for
    /// - `vote_result`: Whether the consensus passed (true) or failed (false)
    ///
    /// ## Returns:
    /// - Vector of Waku messages to send (if any)
    ///
    /// ## State Transitions:
    /// **Steward:**
    /// - **Vote YES**: Voting → ConsensusReached → Waiting → Working (creates and sends batch proposals, then applies them)
    /// - **Vote NO**: Voting → Working (discards proposals)
    ///
    /// **Non-Steward:**
    /// - **Vote YES**: Voting → ConsensusReached → Waiting → Working (waits for consensus + batch proposals, then applies them)
    /// - **Vote NO**: Voting → Working (no proposals to apply)
    ///
    /// ## Effects:
    /// - Completes voting in the group
    /// - Handles proposal application or cleanup based on result
    /// - Manages state transitions for both steward and non-steward users
    /// - Processes pending batch proposals if available
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various state machine and proposal processing errors
    async fn handle_consensus_result(
        &mut self,
        group_name: &str,
        vote_result: bool,
    ) -> Result<Vec<OutboundPacket>, UserError> {
        let group = self.group_ref(group_name).await?;
        group.write().await.complete_voting(vote_result).await?;

        // Handle vote result based on steward status
        if group.read().await.is_steward().await {
            if vote_result {
                // Vote YES: Apply proposals and send commit messages
                info!("[handle_consensus_result]: Vote YES, sending commit message");

                // Apply proposals and complete (state must be ConsensusReached for this)
                let messages = self.apply_proposals(group_name).await?;
                group.write().await.handle_yes_vote().await?;
                Ok(messages)
            } else {
                // Vote NO: Empty proposal queue without applying, no commit messages
                info!(
                    "[handle_consensus_result]: Vote NO, emptying proposal queue without applying"
                );

                // Empty proposals without state requirement (direct steward call)
                group.write().await.handle_no_vote().await?;

                Ok(vec![])
            }
        } else if vote_result {
            // Vote YES: Group already moved to ConsensusReached during complete_voting; transition to Waiting
            {
                let mut group_guard = group.write().await;
                group_guard.start_waiting_after_consensus().await?;
            }
            info!("[handle_consensus_result]: Non-steward user transitioning to Waiting state to await batch proposals");

            // Check if there are pending batch proposals that can now be processed
            if self.pending_batch_proposals.contains(group_name).await {
                info!("[handle_consensus_result]: Non-steward user has pending batch proposals, processing them now");
                let action = self.process_stored_batch_proposals(group_name).await?;
                info!("[handle_consensus_result]: Successfully processed pending batch proposals");
                if let Some(action) = action {
                    match action {
                        UserAction::Outbound(outbound_packet) => {
                            info!("[handle_consensus_result]: Sending waku message to backend");
                            Ok(vec![outbound_packet])
                        }
                        UserAction::LeaveGroup(group_name) => {
                            self.leave_group(group_name.as_str()).await?;
                            info!("[handle_consensus_result]: Non-steward user left group {group_name}");
                            Ok(vec![])
                        }
                        UserAction::DoNothing => {
                            info!("[handle_consensus_result]: No action to process");
                            Ok(vec![])
                        }
                        _ => {
                            error!("[handle_consensus_result]: Invalid action to process");
                            Err(UserError::InvalidUserAction(action.to_string()))
                        }
                    }
                } else {
                    info!("[handle_consensus_result]: No action to process");
                    Ok(vec![])
                }
            } else {
                info!("[handle_consensus_result]: No pending batch proposals to process");
                Ok(vec![])
            }
        } else {
            // Vote NO: Transition to Working state
            group.write().await.start_working().await;
            info!("[handle_consensus_result]: Non-steward user transitioning to Working state after failed vote");
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
                reason,
            } => {
                info!(
                    "[handle_consensus_event]: Consensus failed for proposal {proposal_id} in group {group_name}: {reason}"
                );

                let group = self.group_ref(group_name).await?;

                let current_state = group.read().await.get_state().await;

                info!("[handle_consensus_event]: Handling consensus failure in {:?} state for proposal {proposal_id}", current_state);

                // Handle consensus failure based on current state
                match current_state {
                    GroupState::Voting => {
                        // If we're in Voting state, complete voting with liveness criteria
                        // Get liveness criteria from the actual proposal
                        let liveness_result = self
                            .consensus_service
                            .get_proposal_liveness_criteria(group_name, proposal_id)
                            .await
                            .unwrap_or(false); // Default to false if proposal not found

                        info!("[handle_consensus_result]:Applying liveness criteria for failed proposal {proposal_id}: {liveness_result}");
                        let messages = self
                            .handle_consensus_result(group_name, liveness_result)
                            .await?;
                        Ok(messages)
                    }
                    _ => Err(UserError::InvalidGroupState(current_state.to_string())),
                }
            }
        }
    }

    /// Process incoming consensus proposal.
    ///
    /// ## Parameters:
    /// - `proposal`: The consensus proposal to process
    /// - `group_name`: The name of the group the proposal is for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - Stores proposal in consensus service
    /// - Starts voting phase in the group
    /// - Creates voting proposal for frontend
    ///
    /// ## State Transitions:
    /// - Any state → Voting (starts voting phase)
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various consensus service errors
    pub async fn process_consensus_proposal(
        &mut self,
        proposal: Proposal,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        self.consensus_service
            .process_incoming_proposal(group_name, proposal.clone())
            .await?;

        let group = self.group_ref(group_name).await?;
        group.write().await.start_voting().await?;
        info!(
            "[process_consensus_proposal]: Starting voting for proposal {}",
            proposal.proposal_id
        );

        // Send voting proposal to frontend
        let voting_proposal: AppMessage = VotePayload {
            group_id: group_name.to_string(),
            proposal_id: proposal.proposal_id,
            group_requests: proposal.group_requests.clone(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        }
        .into();

        Ok(UserAction::SendToApp(voting_proposal))
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
                .vote_on_proposal(group_name, proposal_id, user_vote, self.eth_signer.clone())
                .await?;
            proposal.into()
        } else {
            info!(
                "[process_user_vote]: User voting for proposal {proposal_id} in group {group_name}"
            );
            let vote = self
                .consensus_service
                .process_user_vote(group_name, proposal_id, user_vote, self.eth_signer.clone())
                .await?;
            vote.into()
        };

        self.build_group_message(app_message, group_name)
            .await
            .map(UserAction::Outbound)
    }

    /// Process incoming consensus vote and handle immediate state transitions.
    ///
    /// ## Parameters:
    /// - `vote`: The consensus vote to process
    /// - `group_name`: The name of the group the vote is for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - Stores vote in consensus service
    /// - Handles immediate state transitions if consensus is reached
    ///
    /// ## State Transitions:
    /// When consensus is reached immediately after processing a vote:
    /// - **Vote YES**: Non-steward transitions to Waiting state to await batch proposals
    /// - **Vote NO**: Non-steward transitions to Working state immediately
    /// - **Steward**: Relies on event-driven system for full proposal management
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various consensus service errors
    pub(crate) async fn process_consensus_vote(
        &mut self,
        vote: Vote,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        self.consensus_service
            .process_incoming_vote(group_name, vote.clone())
            .await?;

        Ok(UserAction::DoNothing)
    }
}
