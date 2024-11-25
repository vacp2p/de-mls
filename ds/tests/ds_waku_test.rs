use std::{env, str::FromStr};
use std::sync::mpsc::channel;
use alloy::signers::local::PrivateKeySigner;
use waku_bindings::WakuMessage;

use ds::ds_waku::{setup_node_handle, MsgType, WakuGroupClient};

#[tokio::test]
async fn test_waku_client() {
    let node_name = env::var("NODE").unwrap();
    let group_name = "new_group".to_string();
    let msg = "test_message".as_bytes().to_vec();

    let (sender, receiver) = channel::<WakuMessage>();
    let node = setup_node_handle(vec![node_name.clone()]).unwrap();
    let waku_client = WakuGroupClient::new(&node, group_name.clone(), sender).unwrap();

    let topics = waku_client.waku_relay_topics(&node);
    println!("topics: {:?}", topics);

    waku_client
        .send_to_waku(&node, msg.clone(), MsgType::Text)
        .unwrap();

    let msg_clone = msg.clone();
    let received = receiver.recv().unwrap();
    println!("received: {:?}", received);

    assert_eq!(received.payload(), msg_clone);

    waku_client
        .send_to_waku(&node, msg.clone(), MsgType::Text)
        .unwrap();

    let msg_clone = msg.clone();
    let received = receiver.recv().unwrap();
    println!("received: {:?}", received);

    assert_eq!(received.payload(), msg_clone);
}
