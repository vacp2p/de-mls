//! Voting operations (start voting, process user votes).

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::storage::ConsensusStorage;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::{
    app::{GroupState, StateChangeHandler, User, UserError, cast_vote, submit_proposal},
    core::{DeMlsProvider, GroupEventHandler, ProviderConsensus, build_message, group_members},
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{GroupUpdateRequest, group_update_request},
};

use super::GroupEntry;

/// Check whether a `GroupUpdateRequest` is an emergency criteria proposal.
fn is_emergency_proposal(request: &GroupUpdateRequest) -> bool {
    matches!(
        request.payload,
        Some(group_update_request::Payload::EmergencyCriteria(_))
    )
}

/// Cast a YES vote on behalf of the proposal creator, then broadcast it.
///
/// No-op if the proposal has already resolved (not in active list) or the
/// creator already voted. Called from the delayed auto-vote timer.
#[allow(clippy::too_many_arguments)]
async fn cast_creator_auto_yes<P: DeMlsProvider, H: GroupEventHandler + 'static>(
    group_name: &str,
    proposal_id: u32,
    scope: &P::Scope,
    consensus: &Arc<ProviderConsensus<P>>,
    groups: &Arc<RwLock<HashMap<String, GroupEntry>>>,
    handler: &Arc<H>,
    mls_service: &Arc<MlsService<P::Storage>>,
    signer: PrivateKeySigner,
    app_id: &[u8],
) -> Result<(), UserError> {
    // Skip if already resolved.
    let still_active = consensus
        .storage()
        .get_active_proposals(scope)
        .await
        .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
        .unwrap_or(false);
    if !still_active {
        return Ok(());
    }

    // Skip if the creator already voted (check session state before voting).
    if let Ok(p) = consensus.storage().get_proposal(scope, proposal_id).await {
        let already_voted = p
            .votes
            .iter()
            .any(|v| v.vote_owner == mls_service.wallet_bytes());
        if already_voted {
            return Ok(());
        }
    }

    // Clone the Group handle so we can cast without holding the lock.
    let group = {
        let g = groups.read().await;
        let entry = g.get(group_name).ok_or(UserError::GroupNotFound)?;
        entry.group.clone()
    };

    let app_message = cast_vote::<P, _>(&group, proposal_id, true, consensus, signer).await?;
    let packet = build_message(&group, mls_service, &app_message, app_id)?;
    handler.on_outbound(group_name, packet).await?;
    info!("[creator_auto_yes] Auto-YES cast for proposal {proposal_id}");
    Ok(())
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
        let auto_vote_delay = self.default_group_config.creator_auto_vote_delay;
        let consensus = Arc::clone(&self.consensus_service);
        let groups = Arc::clone(&self.groups);
        let handler = Arc::clone(&self.handler);
        let mls_service = Arc::clone(&self.mls_service);
        let eth_signer = self.eth_signer.clone();
        let app_id = self.app_id.clone();
        let group_name_for_error = group_name.clone();

        tokio::spawn(async move {
            let result: Result<u32, UserError> = async {
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

                Ok(proposal_id)
            }
            .await;

            match result {
                Ok(proposal_id) => {
                    // One task, two sequential sleeps — no nested spawn.
                    //
                    // Phase 1 (auto-YES): sleep `auto_vote_delay`, then cast
                    //   creator YES if the proposal is still active and the
                    //   creator hasn't voted manually.
                    // Phase 2 (timeout): sleep the remaining time up to
                    //   `consensus_timeout`, then resolve via the liveness
                    //   criteria if the proposal is still active.
                    //
                    // If auto-vote is disabled, we just sleep the full timeout.
                    let scope = P::Scope::from(group_name.clone());

                    let (phase1, phase2) = match auto_vote_delay {
                        Some(d) if d < consensus_timeout => (Some(d), consensus_timeout - d),
                        // Delay >= timeout is nonsensical; fall back to no auto-vote.
                        _ => (None, consensus_timeout),
                    };

                    if let Some(delay) = phase1 {
                        tokio::time::sleep(delay).await;
                        if let Err(e) = cast_creator_auto_yes::<P, H>(
                            &group_name,
                            proposal_id,
                            &scope,
                            &consensus,
                            &groups,
                            &handler,
                            &mls_service,
                            eth_signer.clone(),
                            &app_id,
                        )
                        .await
                        {
                            info!(
                                "[initiate_proposal]: Creator auto-YES skipped for \
                                 proposal {proposal_id}: {e}"
                            );
                        }
                    }

                    tokio::time::sleep(phase2).await;

                    let still_active = consensus
                        .storage()
                        .get_active_proposals(&scope)
                        .await
                        .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
                        .unwrap_or(false);

                    if still_active {
                        if let Err(e) = consensus
                            .handle_consensus_timeout(&scope, proposal_id)
                            .await
                        {
                            info!(
                                "[initiate_proposal]: Timeout resolution for proposal \
                                 {proposal_id}: {e}"
                            );
                        }
                    }
                }
                Err(err) => {
                    error!("[initiate_proposal]: background task failed: {err}");
                    handler
                        .on_error(&group_name_for_error, "Start voting", &err.to_string())
                        .await;
                }
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

    /// Handle an incoming membership update (KP-derived `InviteMember` or
    /// `RemoveMember`): buffer it so every member has a durable record, then
    /// promote it to a voting proposal if this node is the current epoch
    /// steward and the group is in a state that accepts new proposals.
    ///
    /// Non-epoch-steward members (or epoch stewards in non-Working states)
    /// keep the entry in the buffer; a later epoch rotation will drain it.
    pub async fn handle_incoming_update_request(
        &self,
        group_name: &str,
        request: GroupUpdateRequest,
    ) -> Result<(), UserError> {
        let current_epoch = self.mls_service.current_epoch(group_name).unwrap_or(0);

        // Live-rotation check: "epoch steward" means the first nominal steward
        // in rotation order who is still a group member. This keeps the list
        // stable but lets us skip stewards who have been removed.
        let members_for_rotation = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            if self.mls_service.has_group(entry.group.group_name()) {
                crate::core::group_members(&entry.group, &self.mls_service).unwrap_or_default()
            } else {
                Vec::new()
            }
        };

        let (inserted, is_epoch_steward, state, buffer_total, should_propose) = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

            // Skip buffering if the request carries no membership target
            // (defensive — core only emits membership changes here).
            if crate::core::target_identity_of(&request).is_none() {
                return Ok(());
            }

            let inserted = entry
                .group
                .buffer_pending_update(request.clone(), current_epoch);

            // Only the epoch steward proposes immediately. The buffer survives
            // across freeze rounds so the next epoch steward can retry.
            let is_es = entry
                .group
                .is_live_epoch_steward(current_epoch, &members_for_rotation);
            let state = entry.state_machine.current_state();
            let total = entry.group.pending_update_count();
            let should = is_es && state == GroupState::Working;
            (inserted, is_es, state, total, should)
        };

        info!(
            "[handle_incoming_update_request] group={group_name} epoch={current_epoch} \
             inserted={inserted} buffer_total={buffer_total} is_epoch_steward={is_epoch_steward} \
             state={state} propose={should_propose}"
        );

        if should_propose {
            // check_proposal_allowed may still reject (active emergency etc.);
            // leave the entry in the buffer so the next rotation picks it up.
            if let Err(e) = self
                .initiate_proposal(group_name.to_string(), request)
                .await
            {
                info!(
                    "[handle_incoming_update_request]: initiate_proposal deferred \
                     for group {group_name}: {e}"
                );
            }
        }
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
