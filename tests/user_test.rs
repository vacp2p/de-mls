use alloy::signers::local::PrivateKeySigner;
use de_mls::user::User;
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::{env, str::FromStr};
use tokio::sync::Mutex;
use waku_bindings::WakuMessage;

use ds::ds_waku::{setup_node_handle, MsgType, WakuGroupClient};

#[tokio::test]
async fn test_waku_client_end() {
    let node_name = env::var("NODE").unwrap();
    let group_name = "group_test".to_string();
    let msg = "end".as_bytes().to_vec();

    let user_priv_key2 = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    let signer2 = PrivateKeySigner::from_str(user_priv_key2).unwrap();
    let user_address2 = signer2.address().to_string();
    let node = setup_node_handle(vec![node_name]).unwrap();
    let mut user2 = User::new(user_priv_key2, node).await.unwrap();
    let user2_arc = Arc::new(Mutex::new(user2));

    let (receiver, topics) = user2_arc
        .as_ref()
        .lock()
        .await
        .subscribe_to_group(group_name.clone())
        .await
        .unwrap();
    let msg = format!("Successfully subscribe to group: {:?}", group_name.clone());

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
    h1.await.unwrap();
}
