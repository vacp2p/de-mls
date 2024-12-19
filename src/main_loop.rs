use alloy::signers::local::PrivateKeySigner;
use kameo::actor::{pubsub::PubSub, ActorRef};
use log::{error, info};
use openmls::prelude::AppAckProposal;
use std::{
    str::FromStr,
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Mutex;
use waku_bindings::{Running, WakuMessage, WakuNodeHandle};

use crate::user::{ProcessLeaveGroup, ProcessSendMessage, User, UserAction};
use crate::{
    group_actor::*,
    user::{ProcessAdminMessage, ProcessCreateGroup},
    AppState,
};
use ds::{
    ds_waku::register_handler,
    waku_actor::{self, *},
};

#[derive(Debug, Clone)]
pub struct Connection {
    pub eth_private_key: String,
    pub group_id: String,
    pub should_create_group: bool,
}

pub async fn main_loop(
    connection: Connection,
    app_state: Arc<AppState>,
    // tx_waku: Sender<String>,
    // rx_ws: Receiver<String>,
) -> Result<ActorRef<User>, Box<dyn std::error::Error>> {
    let signer = PrivateKeySigner::from_str(&connection.eth_private_key)?;
    let user_address = signer.address().to_string();
    let group_name: String = connection.group_id.clone();
    // Create user
    let user = User::new(&connection.eth_private_key)?;
    let user_ref = kameo::spawn(user);

    info!(
        "Creating group[{:?}]: {:?}",
        connection.should_create_group, group_name
    );
    user_ref
        .ask(ProcessCreateGroup {
            group_name: group_name.clone(),
            is_creation: connection.should_create_group,
        })
        .await?;
    let mut content_topics = app_state.waku_actor.ask(ProcessSubscribeToGroup {
        group_name: group_name.clone(),
    })
    .await?;
    app_state.content_topics.lock().unwrap().append(&mut content_topics);

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
                let res = user_clone
                    .ask(ProcessAdminMessage {
                        group_name: group_name_clone.clone(),
                    })
                    .await;
                match res {
                    Ok(msg) => {
                        let res = node_clone.ask(msg).await;
                        match res {
                            Ok(id) => {
                                info!("Successfully publish admin message with id: {:?}", id);
                            }
                            Err(e) => {
                                error!("Error sending admin message to waku: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error preparing admin message to waku: {}", e);
                    }
                }
            }
        });
    };

    Ok(user_ref)
}
