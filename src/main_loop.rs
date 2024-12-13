use std::{
    str::FromStr,
    sync::{
        mpsc::{Receiver, Sender},
        Arc,
    }, time::Duration,
};
use tokio::sync::Mutex;
use env_logger;
use log::{info, error};

use alloy::signers::local::PrivateKeySigner;
use ds::ds_waku::setup_node_handle;

use crate::user::{AdminTrait, User};

#[derive(Debug, Clone)]
pub struct Connection {
    pub eth_private_key: String,
    pub group_id: String,
    pub node: String,
    pub is_created: bool,
}

pub async fn main_loop(
    connection: Connection,
    tx: Sender<String>,
    rx: Receiver<String>,
) -> Result<tokio::task::JoinHandle<()>, Box<dyn std::error::Error>> {
    let signer = PrivateKeySigner::from_str(&connection.eth_private_key)?;
    let user_address = signer.address().to_string();

    let nodes_name: Vec<String> = vec![connection.node];
    let group_name: String = connection.group_id.clone();
    let node = setup_node_handle(nodes_name)?;

    info!("Setup node handle for {:?}", user_address);
    // Create user
    let user = User::new(&connection.eth_private_key, node).await?;
    let user_arc = Arc::new(Mutex::new(user));

    let recv = if connection.is_created {
        info!("Subscribe to group for {:?}", user_address);
        user_arc
            .as_ref()
            .lock()
            .await
            .subscribe_to_group(group_name.clone())
            .await?
    } else {
        info!("Create group for {:?}", user_address);
        let receiver = user_arc
            .as_ref()
            .lock()
            .await
            .create_group(group_name.clone())?;
        
        info!("Start admin message for {:?}", user_address);
        let user_clone = user_arc.clone();
        let group_name_clone = group_name.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(20));
            loop {
                interval.tick().await;
                let res = send_admin_msg(&user_clone, group_name_clone.clone()).await;
                match res {
                    Ok(msg) => {
                        info!("Successfully publish message with id: {}", msg);
                    },
                    Err(err) => {
                        error!("Error sending admin message: {}", err);
                    },
                }
            }
        });

        receiver
    };

    

    let user_clone = user_arc.clone();
    let user_address_clone = user_address.clone();
    let mut recv_from_waku = {
        tokio::spawn(async move {
            while let Ok(msg) = recv.recv() {
                info!("Received message from waku for {:?}", user_address_clone);
                let res = user_clone.lock().await.process_waku_msg(msg).await;
                match res {
                    Ok(msg) => {
                        if let Some(msg) = msg {
                            let res = tx.send(msg);
                            if let Err(e) = res {
                                error!("Error sending message to client: {}", e);
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
    let mut recv_from_cli = {
        tokio::spawn(async move {
            info!("Running recv messages from ws for {:?}", user_address_clone);
            while let Ok(msg) = rx.recv() {
                let res = user_clone
                    .as_ref()
                    .lock()
                    .await
                    .send_msg(&msg, group_name.clone())
                    .await;
                if let Err(e) = res {
                    error!("Error sending message to waku: {}", e);
                }
            }
        })
    };

    Ok(tokio::spawn(async move {
        tokio::select! {
            _ = (&mut recv_from_waku) => recv_from_cli.abort(),
            _ = (&mut recv_from_cli) => recv_from_waku.abort(),
        }
    }))
}


async fn send_admin_msg(
    user: &Arc<Mutex<User>>,
    group_name: String,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut user = user.as_ref().lock().await;
    let group = user.groups.get_mut(&group_name.clone()).unwrap();
    group.admin.as_mut().unwrap().generate_new_key_pair()?;
    let admin_msg = group.admin.as_mut().unwrap().generate_admin_message();
    let res = user.send_admin_msg(admin_msg, group_name.clone()).await;
    match res {
        Ok(msg_id) => return Ok(msg_id),
        Err(e) => return Err(e.to_string().into()),
    }
}