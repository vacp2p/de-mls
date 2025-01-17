use kameo::actor::ActorRef;
use log::info;
use tokio_util::sync::CancellationToken;
use waku_bindings::WakuMessage;

use crate::{
    user::{ProcessLeaveGroup, ProcessRemoveUser, ProcessSendMessage, User, UserAction},
    ws_actor::{RawWsMessage, WsAction, WsActor},
    MessageToPrint,
};
use ds::waku_actor::{ProcessUnsubscribeFromGroup, WakuActor};

pub async fn handle_user_actions(
    msg: WakuMessage,
    waku_actor: ActorRef<WakuActor>,
    ws_actor: ActorRef<WsActor>,
    user_actor: ActorRef<User>,
    cancel_token: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let actions = user_actor.ask(msg).await?;
    for action in actions {
        match action {
            UserAction::SendToWaku(msg) => {
                let id = waku_actor.ask(msg).await?;
                info!("Successfully publish message with id: {:?}", id);
            }
            UserAction::SendToGroup(msg) => {
                info!("Send to group: {:?}", msg);
                ws_actor.ask(msg).await?;
            }
            UserAction::RemoveGroup(group_name) => {
                waku_actor
                    .ask(ProcessUnsubscribeFromGroup {
                        group_name: group_name.clone(),
                    })
                    .await?;
                user_actor
                    .ask(ProcessLeaveGroup {
                        group_name: group_name.clone(),
                    })
                    .await?;
                info!("Leave group: {:?}", &group_name);
                ws_actor
                    .ask(MessageToPrint {
                        sender: "system".to_string(),
                        message: format!("Group {} removed you", group_name),
                        group_name: group_name.clone(),
                    })
                    .await?;
                cancel_token.cancel();
            }
            UserAction::DoNothing => {}
        }
    }
    Ok(())
}

pub async fn handle_ws_action(
    msg: RawWsMessage,
    ws_actor: ActorRef<WsActor>,
    user_actor: ActorRef<User>,
    waku_actor: ActorRef<WakuActor>,
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
                .ask(ProcessSendMessage {
                    msg: msg.message,
                    group_name: msg.group_id,
                })
                .await?;
            let id = waku_actor.ask(pmt).await?;
            info!("Successfully publish message with id: {:?}", id);
        }
        WsAction::RemoveUser(user_to_ban, group_name) => {
            info!("Got remove user: {:?}", &user_to_ban);
            let pmt = user_actor
                .ask(ProcessRemoveUser {
                    user_to_ban: user_to_ban.clone(),
                    group_name: group_name.clone(),
                })
                .await?;
            let id = waku_actor.ask(pmt).await?;
            info!("Successfully publish message with id: {:?}", id);
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
