use std::{env, str::FromStr};
use std::sync::mpsc::channel;
use alloy::signers::local::PrivateKeySigner;
use de_mls::user::User;
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

    let (receiver, w_c) = user2.subscribe_to_group(group_name.clone()).await.unwrap();
    let msg = format!("Successfully subscribe to group: {:?}", group_name.clone());
    println!("{}", msg);

    tokio::spawn(async move {
        while let Ok(msg) = receiver.recv() {
            if msg.payload() == "end".as_bytes() {
                break;
            }
            println!("Received message for user2: {:?}", msg);
        }
    });
}