use tracing::{error, info};

use crate::{
    error::UserError,
    protos::{
        consensus::v1::{UpdateRequest, VotePayload},
        de_mls::messages::v1::AppMessage,
    },
    user::{User, UserAction},
};
use ds::transport::OutboundPacket;

impl User {
    /// Check if the user is a steward for the given group.
    /// ## Returns:
    /// - Serialized `UiUpdateRequest` representing the removal request for UI consumption
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to check
    ///
    /// ## Returns:
    /// - `true` if the user is a steward for the group, `false` otherwise
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if the group does not exist
    pub async fn is_user_steward_for_group(&self, group_name: &str) -> Result<bool, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };
        let is_steward = group.read().await.is_steward().await;
        Ok(is_steward)
    }

    /// Check if the MLS group is initialized for the given group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to check
    ///
    /// ## Returns:
    /// - `true` if the MLS group is initialized for the group, `false` otherwise
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if the group does not exist
    pub async fn is_user_mls_group_initialized_for_group(
        &self,
        group_name: &str,
    ) -> Result<bool, UserError> {
        let group = self.group_ref(group_name).await?;
        let is_initialized = group.read().await.is_mls_group_initialized();
        Ok(is_initialized)
    }

    /// Get the current epoch proposals for the given group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to get the proposals for
    ///
    /// ## Returns:
    /// - A vector of `GroupUpdateRequest` representing the current epoch proposals
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if the group does not exist
    pub async fn get_current_epoch_proposals(
        &self,
        group_name: &str,
    ) -> Result<Vec<crate::steward::GroupUpdateRequest>, UserError> {
        let group = self.group_ref(group_name).await?;
        let proposals = group.read().await.get_current_epoch_proposals().await;
        Ok(proposals)
    }

    /// Prepare a steward announcement message for a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to prepare the message for
    ///
    /// ## Returns:
    /// - Waku message containing the steward announcement
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `GroupError::StewardNotSet` if no steward is configured
    pub async fn prepare_steward_msg(
        &mut self,
        group_name: &str,
    ) -> Result<OutboundPacket, UserError> {
        let group = self.group_ref(group_name).await?;
        let msg_to_send = group.write().await.generate_steward_message().await?;
        Ok(msg_to_send)
    }

    /// Start a new steward epoch for the given group. It includes validation of the current state
    /// and the number of proposals. If there are no proposals, it will stay in the Working state and return 0.
    /// If there are proposals, it will change the state to Waiting and return the number of proposals.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to start steward epoch for
    ///
    /// ## Returns:
    /// - Number of proposals that will be voted on (0 if no proposals)
    ///
    /// ## State Transitions:
    /// - **With proposals**: Working → Waiting (returns proposal count)
    /// - **No proposals**: Working → Working (stays in Working, returns 0)
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `GroupError::InvalidStateTransition` if the group is not in the Working state
    /// - `GroupError::StewardNotSet` if no steward is configured
    pub async fn start_steward_epoch(&mut self, group_name: &str) -> Result<usize, UserError> {
        let group = self.group_ref(group_name).await?;
        let proposal_count = group
            .write()
            .await
            .start_steward_epoch_with_validation()
            .await?;

        if proposal_count == 0 {
            info!("[start_steward_epoch]: No proposals to vote on, skipping steward epoch");
        } else {
            info!("[start_steward_epoch]: Started steward epoch with {proposal_count} proposals");
        }

        Ok(proposal_count)
    }
    /// Start voting for the given group, returning the proposal ID.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to start voting for
    ///
    /// ## Returns:
    /// - Tuple of (proposal_id, UserAction) for steward actions
    ///
    /// ## Effects:
    /// - Starts voting phase in the group
    /// - Creates consensus proposal for voting
    /// - Sends voting proposal to frontend
    ///
    /// ## State Transitions:
    /// - **Waiting → Voting**: If proposals found and steward starts voting
    /// - **Waiting → Working**: If no proposals found (edge case fix)
    ///
    /// ## Edge Case Handling:
    /// If no proposals are found during voting phase (rare edge case where proposals
    /// disappear between epoch start and voting), transitions back to Working state
    /// to prevent getting stuck in Waiting state.
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::NoProposalsFound` if no proposals exist
    /// - `ConsensusError::SystemTimeError` if the system time creation fails
    pub async fn get_proposals_for_steward_voting(
        &mut self,
        group_name: &str,
    ) -> Result<(u32, UserAction), UserError> {
        info!(
            "[get_proposals_for_steward_voting]: Getting proposals for steward voting in group {group_name}"
        );

        let group = self.group_ref(group_name).await?;

        // If this is the steward, create proposal with vote and send to group
        if group.read().await.is_steward().await {
            let proposals = group
                .read()
                .await
                .get_proposals_for_voting_epoch_as_ui_update_requests()
                .await;
            if !proposals.is_empty() {
                group.write().await.start_voting().await?;

                // Get group members for expected voters count
                let members = group.read().await.members_identity().await?;
                let participant_ids: Vec<Vec<u8>> = members.into_iter().collect();
                let expected_voters_count = participant_ids.len() as u32;

                // Create consensus proposal
                let proposal = self
                    .consensus_service
                    .create_proposal(
                        group_name,
                        uuid::Uuid::new_v4().to_string(),
                        proposals.clone(),
                        self.identity.identity_string().into(),
                        expected_voters_count,
                        3600, // 1 hour expiration
                        true, // liveness criteria
                    )
                    .await?;

                info!(
                    "[get_proposals_for_steward_voting]: Created consensus proposal with ID {} and {} expected voters",
                    proposal.proposal_id, expected_voters_count
                );

                // Send voting proposal to frontend
                let voting_proposal: AppMessage = VotePayload {
                    group_id: group_name.to_string(),
                    proposal_id: proposal.proposal_id,
                    group_requests: proposal.group_requests.clone(),
                    timestamp: proposal.timestamp,
                }
                .into();

                Ok((proposal.proposal_id, UserAction::SendToApp(voting_proposal)))
            } else {
                error!("[get_proposals_for_steward_voting]: No proposals found");
                Err(UserError::NoProposalsFound)
            }
        } else {
            // Not steward, do nothing
            info!("[get_proposals_for_steward_voting]: Not steward, doing nothing");
            Ok((0, UserAction::DoNothing))
        }
    }

    /// Add a remove proposal into the `steward::current_epoch_proposals` vector for the given group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to add the proposal to
    /// - `identity`: The identity string of the member to remove
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `GroupError::InvalidIdentity` if the identity is invalid
    pub async fn add_remove_proposal(
        &mut self,
        group_name: &str,
        identity: String,
    ) -> Result<UpdateRequest, UserError> {
        let group = self.group_ref(group_name).await?;
        let request = group.write().await.store_remove_proposal(identity).await?;
        Ok(request)
    }

    /// Apply proposals for the given group, returning the batch message(s).
    /// - Creates MLS proposals for all pending group updates
    /// - Commits all proposals to the MLS group
    /// - Generates batch proposals message and welcome message if needed
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to apply proposals for
    ///
    /// ## Returns:
    /// - Vector of Waku messages containing batch proposals and welcome messages
    ///
    /// ## Preconditions:
    /// - Group must be initialized with MLS group
    /// - User must be steward for the group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::MlsGroupNotInitialized` if MLS group not initialized
    /// - `GroupError::StewardNotSet` if no steward is configured
    /// - `GroupError::EmptyProposals` if no proposals exist
    /// - `GroupError::InvalidStateTransition` if the group is not in the Waiting state
    pub async fn apply_proposals(
        &mut self,
        group_name: &str,
    ) -> Result<Vec<OutboundPacket>, UserError> {
        let group = self.group_ref(group_name).await?;

        if !group.read().await.is_mls_group_initialized() {
            return Err(UserError::MlsGroupNotInitialized);
        }

        let messages = group
            .write()
            .await
            .create_batch_proposals_message(&self.provider, self.identity.signer())
            .await?;
        info!("[apply_proposals]: Applied proposals for group {group_name}");
        Ok(messages)
    }
}
