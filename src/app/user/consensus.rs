//! Voting operations (start voting, process user votes).

use std::sync::Arc;

use tracing::{error, info};

use crate::{
    app::{GroupState, StateChangeHandler, User, UserError, cast_vote, submit_proposal},
    core::{DeMlsProvider, GroupEventHandler, build_message, group_members},
    protos::de_mls::messages::v1::{GroupUpdateRequest, group_update_request},
};

/// Check whether a `GroupUpdateRequest` is an emergency criteria proposal.
fn is_emergency_proposal(request: &GroupUpdateRequest) -> bool {
    matches!(
        request.payload,
        Some(group_update_request::Payload::EmergencyCriteria(_))
    )
}

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Check that the group state allows creating a proposal of this type
    /// and return the expected voter count.
    ///
    /// Rules:
    /// - Freezing / Selection → always blocked
    /// - Reelection → only emergency proposals allowed
    /// - Active emergency in group → only emergency proposals allowed (RFC partial freeze)
    async fn check_proposal_allowed(
        &self,
        group_name: &str,
        is_emergency: bool,
    ) -> Result<u32, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let state = entry.state_machine.current_state();

        match state {
            GroupState::Reelection => {
                if !is_emergency {
                    return Err(UserError::GroupBlocked(state.to_string()));
                }
            }
            GroupState::Freezing | GroupState::Selection => {
                return Err(UserError::GroupBlocked(state.to_string()));
            }
            _ => {
                if !is_emergency && entry.group.has_active_emergency() {
                    return Err(UserError::PartialFreeze);
                }
            }
        }

        let members = group_members(&entry.group, &self.mls_service)?;
        Ok(members.len() as u32)
    }

    /// Submit a proposal to the consensus service in the background.
    ///
    /// Network errors are reported via `GroupEventHandler::on_error`
    /// since the caller has already returned.
    fn spawn_proposal_submission(
        &self,
        group_name: String,
        request: GroupUpdateRequest,
        expected_voters: u32,
        is_emergency: bool,
    ) {
        let identity_string = self.mls_service.wallet_hex();
        let proposal_expiration = self.default_group_config.proposal_expiration;
        let consensus_timeout = self.default_group_config.consensus_timeout;
        let consensus = Arc::clone(&self.consensus_service);
        let groups = Arc::clone(&self.groups);
        let handler = Arc::clone(&self.handler);
        let group_name_for_error = group_name.clone();

        tokio::spawn(async move {
            let result: Result<(), UserError> = async {
                let (proposal_id, vote_notification) = submit_proposal::<P>(
                    &group_name,
                    &request,
                    expected_voters,
                    identity_string,
                    &*consensus,
                    proposal_expiration,
                    consensus_timeout,
                )
                .await?;

                // Store ownership before emitting UI notification.
                // This ensures `apply_consensus_outcome` sees `is_owner=true`
                // even if a consensus result arrives immediately.
                {
                    let mut groups = groups.write().await;
                    let entry = groups
                        .get_mut(&group_name)
                        .ok_or(UserError::GroupNotFound)?;
                    entry.group.store_voting_proposal(proposal_id, request);
                    if is_emergency {
                        entry.group.observe_emergency(proposal_id);
                    }
                }

                info!("[initiate_proposal]: Stored voting proposal: {proposal_id}");
                handler
                    .on_app_message(&group_name, vote_notification)
                    .await?;

                Ok(())
            }
            .await;

            if let Err(err) = result {
                error!("[initiate_proposal]: background task failed: {err}");
                handler
                    .on_error(&group_name_for_error, "Start voting", &err.to_string())
                    .await;
            }
        });
    }

    /// Start a consensus vote for a group update request.
    ///
    /// Validates the group state synchronously, then submits to the consensus
    /// service in a background task (network call).
    pub async fn initiate_proposal(
        &self,
        group_name: String,
        request: GroupUpdateRequest,
    ) -> Result<(), UserError> {
        let is_emergency = is_emergency_proposal(&request);
        let expected_voters = self
            .check_proposal_allowed(&group_name, is_emergency)
            .await?;

        self.spawn_proposal_submission(group_name, request, expected_voters, is_emergency);
        Ok(())
    }

    /// Process a user vote.
    pub async fn process_user_vote(
        &mut self,
        group_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

        let state = entry.state_machine.current_state();
        if state == GroupState::Freezing || state == GroupState::Selection {
            return Err(UserError::GroupBlocked(state.to_string()));
        }

        let group = entry.group.clone();
        let app_id = self.app_id.clone();
        drop(groups);

        let app_message = cast_vote::<P, _>(
            &group,
            proposal_id,
            vote,
            &*self.consensus_service,
            self.eth_signer.clone(),
        )
        .await?;
        let packet = build_message(&group, &self.mls_service, &app_message, &app_id)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }
}
