use alloy::signers::local::PrivateKeySigner;
use ds::ds_waku::build_content_topics;
use kameo::actor::ActorRef;
use log::{error, info};
use std::{str::FromStr, sync::Arc, time::Duration};

use crate::user::{ProcessAdminMessage, ProcessCreateGroup, User};
use crate::{AppState, Connection, UserError};

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
        .ask(ProcessCreateGroup {
            group_name: group_name.clone(),
            is_creation: connection.should_create_group,
        })
        .await
        .map_err(|e| UserError::KameoCreateGroupError(e.to_string()))?;

    let mut content_topics = build_content_topics(&group_name);
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
                    app_state.waku_node.send(msg).await?;
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
