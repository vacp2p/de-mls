use hashgraph_like_consensus::protos::consensus::v1::Proposal;
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use openmls::prelude::{DeserializeBytes, MlsMessageIn};
use tracing::info;

use crate::{
    error::UserError,
    group::GroupAction,
    protos::de_mls::messages::v1::{
        AppMessage, BatchProposalsMessage, UpdateRequestList, VotePayload,
    },
    state_machine::GroupState,
    user::{User, UserAction},
};

#[derive(Clone, Default)]
pub(crate) struct PendingBatches {
    inner: Arc<RwLock<HashMap<String, BatchProposalsMessage>>>,
}

impl PendingBatches {
    pub(crate) async fn store(&self, group: &str, batch: BatchProposalsMessage) {
        self.inner.write().await.insert(group.to_string(), batch);
    }

    pub(crate) async fn take(&self, group: &str) -> Option<BatchProposalsMessage> {
        self.inner.write().await.remove(group)
    }

    pub(crate) async fn contains(&self, group: &str) -> bool {
        self.inner.read().await.contains_key(group)
    }
}

impl User {
    /// Process batch proposals message from the steward.
    ///
    /// ## Parameters:
    /// - `batch_msg`: The batch proposals message to process
    /// - `group_name`: The name of the group these proposals are for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - Processes all MLS proposals in the batch
    /// - Applies the commit message to complete the group update
    /// - Transitions group to Working state after successful processing
    ///
    /// ## State Requirements:
    /// - Group must be in Waiting state to process batch proposals
    /// - If not in correct state, stores proposals for later processing
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various MLS processing errors
    pub(crate) async fn process_batch_proposals_message(
        &mut self,
        batch_msg: BatchProposalsMessage,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        // Get the group lock
        let group = self.group_ref(group_name).await?;
        let initial_state = group.read().await.get_state().await;
        if initial_state != GroupState::Waiting {
            info!(
                "[process_batch_proposals_message]: Cannot process batch proposals in {initial_state} state, storing for later processing"
            );
            // Store the batch proposals for later processing
            self.pending_batch_proposals
                .store(group_name, batch_msg)
                .await;
            return Ok(UserAction::DoNothing);
        }

        // Process all proposals before the commit
        for proposal_bytes in batch_msg.mls_proposals {
            let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&proposal_bytes)?;
            let protocol_message = mls_message_in.try_into_protocol_message()?;

            let _res = group
                .write()
                .await
                .process_protocol_msg(protocol_message, &self.provider)
                .await?;
        }

        // Then process the commit message
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&batch_msg.commit_message)?;
        let protocol_message = mls_message_in.try_into_protocol_message()?;

        let res = group
            .write()
            .await
            .process_protocol_msg(protocol_message, &self.provider)
            .await?;

        group.write().await.start_working().await;

        match res {
            GroupAction::GroupAppMsg(msg) => Ok(UserAction::SendToApp(msg)),
            GroupAction::LeaveGroup => Ok(UserAction::LeaveGroup(group_name.to_string())),
            GroupAction::DoNothing => Ok(UserAction::DoNothing),
            GroupAction::GroupProposal(proposal) => {
                self.process_incoming_proposal(group_name, proposal).await
            }
            GroupAction::GroupVote(vote) => {
                self.consensus_service
                    .process_incoming_vote(&group_name.to_string(), vote)
                    .await?;
                Ok(UserAction::DoNothing)
            }
        }
    }

    /// Try to process a batch proposals message that was deferred earlier.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group whose stored batch should be retried
    ///
    /// ## Returns:
    /// - `Some(UserAction)` if a stored batch was processed, `None` otherwise
    ///
    /// ## Effects:
    /// - Checks for a cached `BatchProposalsMessage` and processes it immediately if present
    /// - Removes the stored batch once processing succeeds
    ///
    /// ## Usage:
    /// Call after transitioning into `Waiting` so any deferred steward batch can be replayed.
    pub(crate) async fn process_stored_batch_proposals(
        &mut self,
        group_name: &str,
    ) -> Result<Option<UserAction>, UserError> {
        if self.pending_batch_proposals.contains(group_name).await {
            if let Some(batch_msg) = self.pending_batch_proposals.take(group_name).await {
                info!(
                    "[process_stored_batch_proposals]: Processing stored batch proposals for group {}",
                    group_name
                );
                let action = self
                    .process_batch_proposals_message(batch_msg, group_name)
                    .await?;
                Ok(Some(action))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    /// Process an incoming consensus proposal.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group the proposal is for
    /// - `proposal`: The consensus proposal to process
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - Processes the proposal using the consensus service
    /// - Starts voting for the proposal
    /// - Sends the voting proposal to the frontend
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::ConsensusError` if the proposal processing fails
    pub(crate) async fn process_incoming_proposal(
        &self,
        group_name: &str,
        proposal: Proposal,
    ) -> Result<UserAction, UserError> {
        self.consensus_service
            .process_incoming_proposal(&group_name.to_string(), proposal.clone())
            .await?;

        let group = self.group_ref(group_name).await?;
        group.write().await.start_voting().await?;
        info!(
            "[process_incoming_proposal]: Starting voting for proposal {}",
            proposal.proposal_id
        );

        let update_request_list = UpdateRequestList::decode(proposal.payload.as_slice())?;

        // Send voting proposal to frontend
        let voting_proposal: AppMessage = VotePayload {
            group_id: group_name.to_string(),
            proposal_id: proposal.proposal_id,
            group_requests: update_request_list.update_requests,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs(),
        }
        .into();

        Ok(UserAction::SendToApp(voting_proposal))
    }
}
