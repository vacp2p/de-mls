use kameo::actor::ActorRef;
use log::info;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use waku_bindings::WakuMessage;

use crate::{
    message::wrap_conversation_message_into_application_msg,
    user::{User, UserAction},
    user_actor::{LeaveGroupRequest, RemoveUserRequest, SendGroupMessage},
    ws_actor::{RawWsMessage, WsAction, WsActor},
    AppState,
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
        UserAction::SendToApp(msg) => {
            ws_actor.ask(msg).await?;
        }
        UserAction::LeaveGroup(group_name) => {
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
            let app_message = wrap_conversation_message_into_application_msg(
                format!("You're removed from the group {}", group_name).into_bytes(),
                "system".to_string(),
                group_name.clone(),
            );
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
            let app_message = wrap_conversation_message_into_application_msg(
                msg.message.clone().into_bytes(),
                "me".to_string(),
                msg.group_id.clone(),
            );
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
            user_actor
                .ask(RemoveUserRequest {
                    user_to_ban: user_to_ban.clone(),
                    group_name: group_name.clone(),
                })
                .await?;

            let app_message = wrap_conversation_message_into_application_msg(
                format!(
                    "Remove proposal for user {} added to steward queue",
                    user_to_ban
                )
                .into_bytes(),
                "system".to_string(),
                group_name.clone(),
            );
            ws_actor.ask(app_message).await?;
        }
        WsAction::DoNothing => {}
    }

    Ok(())
}
