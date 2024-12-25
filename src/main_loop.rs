use alloy::signers::local::PrivateKeySigner;
use kameo::actor::ActorRef;
use log::{error, info};
use std::{str::FromStr, sync::Arc, time::Duration};

use crate::user::{ProcessAdminMessage, ProcessCreateGroup, User};
use crate::{AppState, UserError};
use ds::waku_actor::ProcessSubscribeToGroup;

#[derive(Debug, Clone)]
pub struct Connection {
    pub eth_private_key: String,
    pub group_id: String,
    pub should_create_group: bool,
}

pub async fn main_loop(
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
        .ask(ProcessCreateGroup {
            group_name: group_name.clone(),
            is_creation: connection.should_create_group,
        })
        .await
        .map_err(|e| UserError::KameoCreateGroupError(e.to_string()))?;

    let mut content_topics = app_state
        .waku_actor
        .ask(ProcessSubscribeToGroup {
            group_name: group_name.clone(),
        })
        .await?;
    app_state
        .content_topics
        .lock()
        .unwrap()
        .append(&mut content_topics);

    if connection.should_create_group {
        info!(
            "User {:?} start sending admin message for group {:?}",
            user_address, group_name
        );
        let user_clone = user_ref.clone();
        let group_name_clone = group_name.clone();
        let node_clone = app_state.waku_actor.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let res = async {
                    let msg = user_clone
                        .ask(ProcessAdminMessage {
                            group_name: group_name_clone.clone(),
                        })
                        .await
                        .map_err(|e| UserError::KameoSendMessageError(e.to_string()))?;
                    let id = node_clone.ask(msg).await?;
                    info!("Successfully publish admin message with id: {:?}", id);
                    Ok::<(), UserError>(())
                }
                .await;
                if let Err(e) = res {
                    error!("Error sending admin message to waku: {}", e);
                }
            }
        });
    };

    Ok(user_ref)
}
