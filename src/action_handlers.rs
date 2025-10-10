use kameo::actor::ActorRef;
use log::info;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use waku_bindings::WakuMessage;

use crate::{
    protos::messages::v1::{AppMessage, BanRequest, ConversationMessage}, topic_filter::TopicFilter, user::{User, UserAction}, user_actor::{BuildBanMessage, LeaveGroupRequest, SendGroupMessage, UserVoteRequest}, ws_actor::{RawWsMessage, WsAction, WsActor}, AppState
};
use ds::waku_actor::WakuMessageToSend;

pub async fn handle_user_actions(
    msg: WakuMessage,
    waku_node: Sender<WakuMessageToSend>,
    ws_actor: ActorRef<WsActor>,
    user_actor: ActorRef<User>,
    topics: Arc<TopicFilter>,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let action = user_actor.ask(msg).await?;
    match action {
        UserAction::SendToWaku(msg) => {
            waku_node.send(msg).await?;
        }
        UserAction::SendToApp(msg) => {
            ws_actor.ask(msg).await?;
        }
        UserAction::LeaveGroup(group_name) => {
            user_actor
                .ask(LeaveGroupRequest {
                    group_name: group_name.clone(),
                })
                .await?;

            topics.remove_many(&group_name).await;
            info!("Leave group: {:?}", &group_name);
            let app_message: AppMessage = ConversationMessage {
                message: format!("You're removed from the group {group_name}").into_bytes(),
                sender: "SYSTEM".to_string(),
                group_name: group_name.clone(),
            }
            .into();
            ws_actor.ask(app_message).await?;
            cancel_token.cancel();
        }
        UserAction::DoNothing => {}
    }
    Ok(())
}

pub async fn handle_ws_action(
    msg: RawWsMessage,
    ws_actor: ActorRef<WsActor>,
    user_actor: ActorRef<User>,
    waku_node: Sender<WakuMessageToSend>,
) -> Result<(), Box<dyn std::error::Error>> {
    let action = ws_actor.ask(msg).await?;
    match action {
        WsAction::Connect(connect) => {
            info!("Got unexpected connect: {:?}", &connect);
        }
        WsAction::UserMessage(msg) => {
            let app_message: AppMessage = ConversationMessage {
                message: msg.message.clone(),
                sender: "me".to_string(),
                group_name: msg.group_id.clone(),
            }
            .into();
            ws_actor.ask(app_message).await?;

            let pmt = user_actor
                .ask(SendGroupMessage {
                    message: msg.message.clone(),
                    group_name: msg.group_id,
                })
                .await?;
            waku_node.send(pmt).await?;
        }
        WsAction::RemoveUser(user_to_ban, group_name) => {
            info!("Got remove user: {:?}", &user_to_ban);

            // Create a ban request message to send to the group
            let ban_request_msg = BanRequest {
                user_to_ban: user_to_ban.clone(),
                requester: "someone".to_string(), // The current user is the requester
                group_name: group_name.clone(),
            };

            // Send the ban request directly via Waku if the user is not the steward
            // If steward, need to add remove proposal to the group and sent notification to the group
            let waku_msg = user_actor
                .ask(BuildBanMessage {
                    ban_request: ban_request_msg,
                    group_name: group_name.clone(),
                })
                .await?;
            waku_node.send(waku_msg).await?;

            // Send a local confirmation message
            let app_message: AppMessage = ConversationMessage {
                message: format!("Ban request for user {user_to_ban} sent to group").into_bytes(),
                sender: "system".to_string(),
                group_name: group_name.clone(),
            }
            .into();
            ws_actor.ask(app_message).await?;
        }
        WsAction::UserVote {
            proposal_id,
            vote,
            group_id,
        } => {
            info!("Got user vote: proposal_id={proposal_id}, vote={vote}, group={group_id}");

            // Process the user vote:
            // if it come from the user, send the vote result to Waku
            // if it come from the steward, just process it and return None
            let user_vote_result = user_actor
                .ask(UserVoteRequest {
                    group_name: group_id.clone(),
                    proposal_id,
                    vote,
                })
                .await?;

            // Send a local confirmation message
            let app_message: AppMessage = ConversationMessage {
                message: format!(
                    "Your vote ({}) has been submitted for proposal {proposal_id}",
                    if vote { "YES" } else { "NO" },
                )
                .into_bytes(),
                sender: "SYSTEM".to_string(),
                group_name: group_id.clone(),
            }
            .into();
            ws_actor.ask(app_message).await?;

            // Send the vote result to Waku
            if let Some(waku_msg) = user_vote_result {
                waku_node.send(waku_msg).await?;
            }
        }
        WsAction::DoNothing => {}
    }

    Ok(())
}
