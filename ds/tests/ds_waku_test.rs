use std::{env, sync::mpsc};

use ds::ds_waku::WakuClient;
use tracing::debug;
use waku_bindings::WakuMessage;

#[tokio::test]
async fn test_waku_client() {
    let node = env::var("NODE").unwrap();
    debug!("Node: {}", node);

    let (sender, receiver) = tokio::sync::broadcast::channel::<WakuMessage>(16);
    let waku_client = WakuClient::new_with_group(vec![node], "test".to_string(), sender).unwrap();

    let msg = "hi".as_bytes().to_vec();
    waku_client
        .send_to_waku(
            msg.clone(),
            waku_client.pubsub_topic.clone(),
            waku_client.content_topics.lock().unwrap()[0].clone(),
        )
        .unwrap();
    let mut receiver = receiver;
    let msg_clone = msg.clone();
    let received = receiver.recv().await.unwrap();

    assert_eq!(received.payload(), msg_clone);
}
