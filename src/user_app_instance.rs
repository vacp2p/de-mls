use alloy::signers::local::PrivateKeySigner;
use ds::build_content_topics;
use kameo::actor::ActorRef;
use log::{error, info};
use std::{str::FromStr, sync::Arc, time::Duration};

use crate::user::{User, UserAction};
use crate::user_actor::{
    ApplyProposalsAndCompleteRequest, CompleteVotingRequest, CreateGroupRequest,
    EmptyProposalsAndCompleteRequest, StartStewardEpochRequest, StartVotingRequest,
    StewardMessageRequest,
};
use crate::{error::UserError, AppState, Connection};

pub const STEWARD_EPOCH: u64 = 60;

pub async fn create_user_instance(
    connection: Connection,
    app_state: Arc<AppState>,
) -> Result<ActorRef<User>, UserError> {
    let signer = PrivateKeySigner::from_str(&connection.eth_private_key)?;
    let user_address = signer.address().to_string();
    let group_name: String = connection.group_id.clone();
    // Create user
    let user = User::new(&connection.eth_private_key)?;
    let user_ref = kameo::spawn(user);
    user_ref
        .ask(CreateGroupRequest {
            group_name: group_name.clone(),
            is_creation: connection.should_create_group,
        })
        .await
        .map_err(|e| UserError::UnableToCreateGroup(e.to_string()))?;

    let mut content_topics = build_content_topics(&group_name);
    info!("Building content topics: {content_topics:?}");
    app_state
        .content_topics
        .write()
        .await
        .append(&mut content_topics);

    if connection.should_create_group {
        info!("User {user_address:?} start sending steward message for group {group_name:?}");
        let user_clone = user_ref.clone();
        let group_name_clone = group_name.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(STEWARD_EPOCH));
            loop {
                interval.tick().await;
                let _ = async {
                    handle_steward_flow_per_epoch(
                        user_clone.clone(),
                        group_name_clone.clone(),
                        app_state.clone(),
                    )
                    .await
                    .map_err(|e| UserError::UnableToHandleStewardEpoch(e.to_string()))?;
                    Ok::<(), UserError>(())
                }
                .await
                .inspect_err(|e| error!("Error sending steward message to waku: {e}"));
            }
        });
    };

    Ok(user_ref)
}

/// Enhanced steward epoch flow with state machine:
/// 1. Start steward epoch (collect pending proposals, change state to Waiting if there are proposals)
/// 2. Send new steward key to the waku node
/// 3. If there are proposals, start voting process (change state to Voting)
/// 4. Complete voting (change state based on result)
/// 5. If vote passed, apply proposals and complete (change state back to Working)
pub async fn handle_steward_flow_per_epoch(
    user: ActorRef<User>,
    group_name: String,
    app_state: Arc<AppState>,
) -> Result<(), UserError> {
    info!("Starting steward epoch for group: {group_name}");

    // Step 1: Start steward epoch - check for proposals and start epoch if needed
    let proposals_count = user
        .ask(StartStewardEpochRequest {
            group_name: group_name.clone(),
        })
        .await
        .map_err(|e| UserError::GetGroupUpdateRequestsError(e.to_string()))?;

    // Step 2: Send new steward key to the waku node for new epoch
    let msg = user
        .ask(StewardMessageRequest {
            group_name: group_name.clone(),
        })
        .await
        .map_err(|e| UserError::ProcessStewardMessageError(e.to_string()))?;
    app_state.waku_node.send(msg).await?;

    if proposals_count == 0 {
        info!("No proposals to vote on for group: {group_name}, completing epoch without voting");
        info!("Steward epoch completed for group: {group_name} (no proposals)");
        return Ok(());
    }

    info!("Found {proposals_count} proposals to vote on for group: {group_name}");

    // Step 3: Start voting process - steward creates proposal with vote and sends to group
    let (proposal_id, action) = user
        .ask(StartVotingRequest {
            group_name: group_name.clone(),
        })
        .await
        .map_err(|e| UserError::UnableToStartVoting(e.to_string()))?;

    match action {
        UserAction::SendToWaku(waku_msg) => {
            app_state.waku_node.send(waku_msg).await?;
        }
        UserAction::DoNothing => {
            info!("No action to take for group: {group_name}");
            return Ok(());
        }
        _ => {
            return Err(UserError::InvalidUserAction(action.to_string()));
        }
    }
    info!("Started voting with vote_id: {proposal_id:?} for group: {group_name}");

    // Step 4: Wait for consensus to be reached (2n/3 votes)
    // This will wait for actual consensus signals from the consensus service
    info!("Waiting for consensus to be reached for group: {group_name}");

    let vote_result = user
        .ask(CompleteVotingRequest {
            group_name: group_name.clone(),
            proposal_id,
        })
        .await
        .map_err(|e| UserError::UnableToCompleteVoting(e.to_string()))?;

    info!("Consensus reached with result: {vote_result} for group: {group_name}");

    // Step 5: If vote passed, apply proposals and complete
    if vote_result {
        let msgs = user
            .ask(ApplyProposalsAndCompleteRequest {
                group_name: group_name.clone(),
            })
            .await
            .map_err(|e| UserError::UnableToApplyProposals(e.to_string()))?;

        // Only send messages if there are any (when there are proposals)
        for msg in msgs {
            app_state.waku_node.send(msg).await?;
        }

        info!("Proposals applied and steward epoch completed for group: {group_name}");
    } else {
        info!("Vote failed, returning to working state for group: {group_name}");
    }

    user.ask(EmptyProposalsAndCompleteRequest {
        group_name: group_name.clone(),
    })
    .await
    .map_err(|e| UserError::EmptyProposalsAndCompleteError(e.to_string()))?;

    info!("Removing proposals and completing steward epoch for group: {group_name}");
    Ok(())
}
