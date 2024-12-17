use alloy::signers::local::PrivateKeySigner;
use log::{error, info};
use std::{
    str::FromStr,
    sync::{
        mpsc::{Receiver, Sender},
        Arc,
    },
    time::Duration,
};
use tokio::sync::Mutex;
use waku_bindings::{Running, WakuNodeHandle};

use crate::user::{User, UserAction};

#[derive(Debug, Clone)]
pub struct Connection {
    pub eth_private_key: String,
    pub group_id: String,
    pub should_create_group: bool,
}

pub async fn main_loop(
    connection: Connection,
    node: Arc<WakuNodeHandle<Running>>,
    tx_waku: Sender<String>,
    rx_ws: Receiver<String>,
) -> Result<tokio::task::JoinHandle<()>, Box<dyn std::error::Error>> {
    let signer = PrivateKeySigner::from_str(&connection.eth_private_key)?;
    let user_address = signer.address().to_string();
    let group_name: String = connection.group_id.clone();
    // Create user
    let user = User::new(&connection.eth_private_key)?;
    let user_arc = Arc::new(Mutex::new(user));

    let recv = if !connection.should_create_group {
        info!(
            "User {:?} subscribe to group {:?}",
            user_address, group_name
        );
        user_arc
            .as_ref()
            .lock()
            .await
            .subscribe_to_group(group_name.clone())?
    } else {
        info!("User {:?} create group {:?}", user_address, group_name);
        let receiver = user_arc
            .as_ref()
            .lock()
            .await
            .create_group(group_name.clone())?;

        info!(
            "User {:?} start sending admin message for group {:?}",
            user_address, group_name
        );
        let user_clone = user_arc.clone();
        let group_name_clone = group_name.clone();
        let node_clone = node.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                let res = user_clone
                    .lock()
                    .await
                    .prepare_admin_msg(group_name_clone.clone())
                    .await;
                match res {
                    Ok(msg) => {
                        let res = user_clone
                            .lock()
                            .await
                            .waku_clients
                            .get(&group_name_clone)
                            .unwrap()
                            .send_to_waku(node_clone.as_ref(), msg);
                        match res {
                            Ok(id) => {
                                info!("Successfully publish admin message with id: {:?}", id);
                            }
                            Err(e) => {
                                error!("Error sending admin message to waku: {}", e);
                            }
                        }
                    }
                    Err(err) => {
                        error!("Error preparing admin message: {}", err);
                    }
                }
            }
        });

        receiver
    };

    let user_clone = user_arc.clone();
    let group_name_clone = group_name.clone();
    let node_clone = node.clone();
    let mut recv_from_waku = {
        tokio::spawn(async move {
            while let Ok(msg) = recv.recv() {
                let res = user_clone.lock().await.process_waku_msg(msg).await;
                match res {
                    Ok(actions) => {
                        for action in actions {
                            match action {
                                UserAction::SendToWaku(msg) => {
                                    let res = user_clone
                                        .lock()
                                        .await
                                        .waku_clients
                                        .get(&group_name_clone)
                                        .unwrap()
                                        .send_to_waku(node_clone.as_ref(), msg);
                                    match res {
                                        Ok(id) => {
                                            info!("Successfully publish message with id: {:?}", id);
                                        }
                                        Err(e) => {
                                            error!("Error sending message to waku: {}", e);
                                        }
                                    }
                                }
                                UserAction::SendToGroup(msg) => {
                                    let res = tx_waku.send(msg);
                                    if let Err(e) = res {
                                        error!("Error sending message to client: {}", e);
                                    }
                                }
                                UserAction::DoNothing => {}
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error processing waku message: {}", e);
                    }
                }
            }
        })
    };

    let user_clone = user_arc.clone();
    let user_address_clone = user_address.clone();
    let group_name_clone = group_name.clone();
    let node_clone = node.clone();
    let mut recv_from_cli = {
        tokio::spawn(async move {
            info!("Running recv messages from ws for {:?}", user_address_clone);
            while let Ok(msg) = rx_ws.recv() {
                let res = user_clone
                    .as_ref()
                    .lock()
                    .await
                    .send_msg(&msg, group_name_clone.clone())
                    .await;
                match res {
                    Ok(msg) => {
                        let res = user_clone
                            .lock()
                            .await
                            .waku_clients
                            .get(&group_name_clone)
                            .unwrap()
                            .send_to_waku(node_clone.as_ref(), msg);
                        match res {
                            Ok(id) => {
                                info!("Successfully publish message with id: {:?}", id);
                            }
                            Err(e) => {
                                error!("Error sending message to waku: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error sending message to waku: {}", e);
                    }
                }
            }
        })
    };

    Ok(tokio::spawn(async move {
        tokio::select! {
            _ = (&mut recv_from_cli) => {
                info!("recv_from_cli finished");
                recv_from_waku.abort();
            }
            _ = (&mut recv_from_waku) => {
                info!("recv_from_waku finished");
                recv_from_cli.abort();
            }
        }
    }))
}
