use alloy::signers::local::PrivateKeySigner;
use ds::build_content_topics;
use kameo::actor::ActorRef;
use log::{error, info};
use std::{str::FromStr, sync::Arc, time::Duration};

use crate::user::*;
use crate::{AppState, Connection, UserError};

pub const ADMIN_EPOCH: u64 = 30;

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
        .map_err(|e| UserError::KameoCreateGroupError(e.to_string()))?;

    let mut content_topics = build_content_topics(&group_name);
    info!("Building content topics: {:?}", content_topics);
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
            let mut interval = tokio::time::interval(Duration::from_secs(ADMIN_EPOCH));
            loop {
                interval.tick().await;
                let _ = async {
                    handle_admin_flow_per_epoch(
                        user_clone.clone(),
                        group_name_clone.clone(),
                        app_state.clone(),
                    )
                    .await
                    .map_err(|e| UserError::KameoSendMessageError(e.to_string()))?;
                    Ok::<(), UserError>(())
                }
                .await
                .inspect_err(|e| error!("Error sending admin message to waku: {}", e));
            }
        });
    };

    Ok(user_ref)
}

/// Each epoch admin of the group will send the admin key to the waku node, which will be used to share
/// key packages for current group and for the current epoch admin do next steps:
/// 1. Get all collected key packages from previous epoch (not processed yet, just drained messaged queue)
/// 2. Send new admin key to the waku node for new epoch and next message will be saved in the messaged queue
/// 3. Process the income key packages from previous epoch and send welcome message to the new members and
///     update message to the other members
pub async fn handle_admin_flow_per_epoch(
    user: ActorRef<User>,
    group_name: String,
    app_state: Arc<AppState>,
) -> Result<(), UserError> {
    // Move all income key packages to processed queue
    let key_packages = user
        .ask(GetIncomeKeyPackagesRequest {
            group_name: group_name.clone(),
        })
        .await
        .map_err(|e| UserError::GetIncomeKeyPackagesError(e.to_string()))?;

    // Send new admin key to the waku node for new epoch and next message will be saved in the messaged queue
    let msg = user
        .ask(AdminMessageRequest {
            group_name: group_name.clone(),
        })
        .await
        .map_err(|e| UserError::ProcessAdminMessageError(e.to_string()))?;
    app_state.waku_node.send(msg).await?;

    // Process the income key packages from previous epoch and send welcome message to the new members and
    // update message to the other members
    if !key_packages.is_empty() {
        let msgs = user
            .ask(InviteUsersRequest {
                group_name: group_name.clone(),
                users: key_packages,
            })
            .await
            .map_err(|e| UserError::ProcessInviteUsersError(e.to_string()))?;
        for msg in msgs {
            app_state.waku_node.send(msg).await?;
        }
    }

    Ok(())
}
