use alloy::signers::local::PrivateKeySigner;
use ds::build_content_topics;
use kameo::actor::ActorRef;
use log::{error, info};
use std::{str::FromStr, sync::Arc, time::Duration};

use crate::user::{User, UserAction};
use crate::user_actor::{
    ConsensusEventMessage, CreateGroupRequest, GetProposalsForStewardVotingRequest,
    StartStewardEpochRequest, StewardMessageRequest,
};
use crate::ws_actor::WsActor;
use crate::LocalSigner;
use crate::{error::UserError, AppState, Connection};

pub const STEWARD_EPOCH: u64 = 15;

pub async fn create_user_instance(
    connection: Connection,
    app_state: Arc<AppState>,
    ws_actor: ActorRef<WsActor>,
) -> Result<ActorRef<User>, UserError> {
    let signer = PrivateKeySigner::from_str(&connection.eth_private_key)?;
    let user_address = signer.address_string();
    let group_name: String = connection.group_id.clone();
    // Create user
    let user = User::new(&connection.eth_private_key)?;

    // Set up consensus event forwarding before spawning the actor
    let consensus_events = user.subscribe_to_consensus_events();

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
        let app_state_steward = app_state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(STEWARD_EPOCH));
            loop {
                interval.tick().await;
                let _ = async {
                    handle_steward_flow_per_epoch(
                        user_clone.clone(),
                        group_name_clone.clone(),
                        app_state_steward.clone(),
                        ws_actor.clone(),
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

    // Set up consensus event forwarding loop
    let user_ref_consensus = user_ref.clone();
    let mut consensus_events_receiver = consensus_events;
    let app_state_consensus = app_state.clone();
    tokio::spawn(async move {
        info!("Starting consensus event forwarding loop");
        while let Ok((group_name, event)) = consensus_events_receiver.recv().await {
            info!("Forwarding consensus event for group {group_name}: {event:?}");
            let result = user_ref_consensus
                .ask(ConsensusEventMessage {
                    group_name: group_name.clone(),
                    event,
                })
                .await;

            match result {
                Ok(commit_messages) => {
                    // Send commit messages to Waku if any
                    if !commit_messages.is_empty() {
                        info!(
                            "Sending {} commit messages to Waku for group {}",
                            commit_messages.len(),
                            group_name
                        );
                        for msg in commit_messages {
                            if let Err(e) = app_state_consensus.waku_node.send(msg).await {
                                error!("Error sending commit message to Waku: {e}");
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error forwarding consensus event: {e}");
                }
            }
        }
        info!("Consensus event forwarding loop ended");
    });

    Ok(user_ref)
}

/// Enhanced steward epoch flow with state machine:
///
/// ## Complete Flow Steps:
/// 1. **Start steward epoch**: Collect pending proposals
///    - If no proposals: Stay in Working state, complete epoch without voting
///    - If proposals exist: Transition Working → Waiting
/// 2. **Send steward key**: Broadcast new steward key to waku network
/// 3. **Voting phase** (only if proposals exist):
///    - Get proposals for voting: Transition Waiting → Voting
///    - If no proposals found (edge case): Transition Waiting → Working, complete epoch
///    - If proposals found: Send voting proposal to group members
/// 4. **Complete voting**: Handle consensus result
///    - Vote YES: Transition Voting → Waiting → Working (after applying proposals)
///    - Vote NO: Transition Voting → Working (proposals discarded)
/// 5. **Apply proposals** (only if vote passed): Execute group changes and return to Working
///
/// ## State Guarantees:
/// - Steward always returns to Working state after epoch completion
/// - No proposals scenario never leaves Working state
/// - All edge cases properly handled with state transitions
pub async fn handle_steward_flow_per_epoch(
    user: ActorRef<User>,
    group_name: String,
    app_state: Arc<AppState>,
    ws_actor: ActorRef<WsActor>,
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
    } else {
        info!("Found {proposals_count} proposals to vote on for group: {group_name}");

        // Step 3: Start voting process - steward gets proposals for voting
        let action = user
            .ask(GetProposalsForStewardVotingRequest {
                group_name: group_name.clone(),
            })
            .await
            .map_err(|e| UserError::UnableToStartVoting(e.to_string()))?;

        // Step 4: Send proposals to ws to steward to vote or do nothing if no proposals
        // After voting, steward sends vote and proposal to waku node and start consensus process
        match action {
            UserAction::SendToApp(app_msg) => {
                info!("Sending app message to ws");
                ws_actor.ask(app_msg).await.map_err(|e| {
                    UserError::UnableToSendMessageToWs(format!("Failed to send message to ws: {e}"))
                })?;
            }
            UserAction::DoNothing => {
                info!("No action to take for group: {group_name}");
                return Ok(());
            }
            _ => {
                return Err(UserError::InvalidUserAction(action.to_string()));
            }
        }
    }

    Ok(())
}
