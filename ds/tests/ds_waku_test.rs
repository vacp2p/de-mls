use std::env;
use std::sync::mpsc::channel;
use waku_bindings::WakuMessage;

use ds::ds_waku::{setup_node_handle, MsgType, WakuConfig, WakuGroupClient, APP_MSG_SUBTOPIC};

#[tokio::test]
async fn test_setup_node_handle() {
    let cfg = WakuConfig::new_from_toml("../resources/cfg_test.toml").unwrap();
    let node = setup_node_handle(cfg.nodes).unwrap();
    let res = node.peers();
    assert!(res.is_ok());
    let peers = res.unwrap();
    println!("peers: {:?}", peers);
    assert_eq!(peers.len(), 1);
}

#[tokio::test]
async fn test_waku_client() {
    let cfg = WakuConfig::new_from_toml("../resources/cfg_test.toml").unwrap();
    let group_name = "new_group".to_string();
    let msg = "test_message".as_bytes().to_vec();

    let (sender_alice, receiver_alice) = channel::<WakuMessage>();
    let (sender_bob, receiver_bob) = channel::<WakuMessage>();
    let node = setup_node_handle(cfg.nodes.clone()).unwrap();
    let waku_client_alice = WakuGroupClient::new(&node, group_name.clone(), sender_alice).unwrap();
    let waku_client_bob = WakuGroupClient::new(&node, group_name.clone(), sender_bob).unwrap();

    let topics = waku_client_alice.waku_relay_topics(&node);
    println!("topics: {:?}", topics);

    waku_client_alice
        .send_to_waku(&node, msg.clone(), APP_MSG_SUBTOPIC)
        .unwrap();

    let msg_clone = msg.clone();
    let res = receiver_alice.recv();    
    println!("received: {:?}", res.err());


    waku_client_bob
        .send_to_waku(&node, msg.clone(), APP_MSG_SUBTOPIC)
        .unwrap();

    let msg_clone = msg.clone();
    let received = receiver_bob.recv().unwrap();
    println!("received: {:?}", received);

    assert_eq!(received.payload(), msg_clone);
}
