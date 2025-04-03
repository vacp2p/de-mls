use kameo::actor::ActorRef;
use log::info;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use waku_bindings::WakuMessage;

use crate::{
    user::{LeaveGroupRequest, RemoveUserRequest, SendGroupMessage, User, UserAction},
    ws_actor::{RawWsMessage, WsAction, WsActor},
    AppState, MessageToPrint,
};
use ds::waku_actor::WakuMessageToSend;

pub async fn handle_user_actions(
    msg: WakuMessage,
    waku_node: Sender<WakuMessageToSend>,
    ws_actor: ActorRef<WsActor>,
    user_actor: ActorRef<User>,
    app_state: Arc<AppState>,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let action = user_actor.ask(msg).await?;
    match action {
        UserAction::SendToWaku(msg) => {
            waku_node.send(msg).await?;
        }
        UserAction::SendToGroup(msg) => {
            info!("Send to group: {:?}", msg);
            ws_actor.ask(msg).await?;
        }
        UserAction::RemoveGroup(group_name) => {
            user_actor
                .ask(LeaveGroupRequest {
                    group_name: group_name.clone(),
                })
                .await?;

            app_state
                .content_topics
                .lock()
                .unwrap()
                .retain(|topic| topic.application_name != group_name);
            info!("Leave group: {:?}", &group_name);
            ws_actor
                .ask(MessageToPrint {
                    sender: "system".to_string(),
                    message: format!("You're removed from the group {}", group_name),
                    group_name: group_name.clone(),
                })
                .await?;
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
            info!("Got user message: {:?}", &msg);
            let mtp = MessageToPrint {
                message: msg.message.clone(),
                group_name: msg.group_id.clone(),
                sender: "me".to_string(),
            };
            ws_actor.ask(mtp).await?;

            let pmt = user_actor
                .ask(SendGroupMessage {
                    msg: msg.message,
                    group_name: msg.group_id,
                })
                .await?;
            waku_node.send(pmt).await?;
        }
        WsAction::RemoveUser(user_to_ban, group_name) => {
            info!("Got remove user: {:?}", &user_to_ban);
            let pmt = user_actor
                .ask(RemoveUserRequest {
                    user_to_ban: user_to_ban.clone(),
                    group_name: group_name.clone(),
                })
                .await?;
            waku_node.send(pmt).await?;
            ws_actor
                .ask(MessageToPrint {
                    sender: "system".to_string(),
                    message: format!("User {} was removed from group", user_to_ban),
                    group_name: group_name.clone(),
                })
                .await?;
        }
        WsAction::DoNothing => {}
    }

    Ok(())
}
