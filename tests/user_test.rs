use alloy::signers::local::PrivateKeySigner;
use de_mls::user::User;
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::{env, str::FromStr};
use tokio::sync::Mutex;
use waku_bindings::WakuMessage;

use ds::ds_waku::{setup_node_handle, MsgType, WakuGroupClient, WakuConfig};

#[tokio::test]
async fn test_waku_client_end() {
    let cfg = WakuConfig::new_from_toml("../resources/cfg_test.toml").unwrap();
    let node_name = cfg.nodes[0].clone();
    let group_name = "new_group".to_string();
    let msg = "test message".to_string();

    let user_priv_key2 = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let signer2 = PrivateKeySigner::from_str(user_priv_key2).unwrap();
    let user_address2 = signer2.address().to_string();
    let node = setup_node_handle(vec![node_name]).unwrap();
    let mut user2 = User::new(user_priv_key2, node).await.unwrap();
    let user2_arc = Arc::new(Mutex::new(user2));

    println!("Subscribing to group: {:?}", group_name.clone());

    let (receiver) = user2_arc
        .as_ref()
        .lock()
        .await
        .subscribe_to_group(group_name.clone())
        .await
        .unwrap();
    println!("Successfully subscribe to group: {:?}", group_name.clone());

    let topics = user2_arc
        .as_ref()
        .lock()
        .await
        .waku_node
        .relay_topics()
        .unwrap();
    println!("Topics: {:?}", topics);

    let group_name_clone = group_name.clone();
    let user_recv_clone = user2_arc.clone();
    let h1 = tokio::spawn(async move {
        while let Ok(msg) = receiver.recv() {
            println!("Received message for user2: {:?}", msg);
            // let mut user = user_recv_clone.as_ref().lock().await;
            // let res = user.process_waku_msg(group_name_clone.clone(), msg).await;
            // match res {
            //     Ok(_) => println!("Successfully process message from receiver"),
            //     Err(e) => println!("Failed to process message: {:?}", e),
            // }
        }
    });

    user2_arc
        .as_ref()
        .lock()
        .await
        .send_msg(&msg, group_name.clone())
        .await
        .unwrap();

    h1.await.unwrap();
}
