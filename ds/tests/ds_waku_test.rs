use std::sync::mpsc::channel;
use waku_bindings::WakuMessage;

use ds::ds_waku::{setup_node_handle, MessageToSend, WakuGroupClient, APP_MSG_SUBTOPIC};

#[tokio::test]
async fn test_waku_client() {
    let group_name = "new_group".to_string();
    let msg = "test_message".as_bytes().to_vec();

    let (sender_alice, _) = channel::<WakuMessage>();
    let (sender_bob, receiver_bob) = channel::<WakuMessage>();
    let node = setup_node_handle(vec![
        "/ip4/143.110.189.47/tcp/60000/p2p/16Uiu2HAmGqxbqiaur7qqXqWyfDrhmKcnR2ZjCj6Whs17jsgZ66xJ"
            .to_string(),
    ])
    .unwrap();
    let waku_client_alice = WakuGroupClient::new(group_name.clone(), sender_alice).unwrap();
    let _ = WakuGroupClient::new(group_name.clone(), sender_bob).unwrap();

    let topics = waku_client_alice.waku_relay_topics(&node);
    println!("topics: {:?}", topics);

    let res = waku_client_alice
        .send_to_waku(
            &node,
            MessageToSend {
                msg: msg.clone(),
                subtopic: APP_MSG_SUBTOPIC.to_string(),
            },
        )
        .unwrap();
    println!("Alice sent message to waku: {:?}", res);

    let received = receiver_bob.recv().unwrap();
    println!("Bob received message from waku: {:?}", received);

    assert_eq!(received.payload(), msg);
}
