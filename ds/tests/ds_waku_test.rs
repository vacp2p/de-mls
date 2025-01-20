use kameo::{
    actor::pubsub::PubSub,
    message::{Context, Message},
    Actor,
};
use log::{error, info};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc::channel;
use waku_bindings::WakuMessage;

use ds::{
    ds_waku::{
        build_content_topics, match_content_topic, setup_node_handle, APP_MSG_SUBTOPIC,
        GROUP_VERSION,
    },
    waku_actor::{ProcessMessageToSend, ProcessSubscribeToGroup, WakuActor},
    DeliveryServiceError,
};
use waku_bindings::waku_set_event_callback;

#[derive(Debug, Clone, Actor)]
pub struct ActorA {
    pub app_id: String,
}

impl ActorA {
    pub fn new() -> Self {
        let app_id = uuid::Uuid::new_v4().to_string();
        Self { app_id }
    }
}

impl Message<WakuMessage> for ActorA {
    type Reply = Result<WakuMessage, DeliveryServiceError>;

    async fn handle(
        &mut self,
        msg: WakuMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        println!("ActorA received message: {:?}", msg.timestamp());
        Ok(msg)
    }
}

#[tokio::test]
async fn test_waku_client() {
    let group_name = "new_group".to_string();
    let mut pubsub = PubSub::<WakuMessage>::new();
    let (sender_alice, mut receiver_alice) = channel::<WakuMessage>(100);
    let node_name = std::env::var("NODE").expect("NODE is not set");
    let res = setup_node_handle(vec![node_name]);
    assert!(res.is_ok());
    let node = res.unwrap();
    let uuid = uuid::Uuid::new_v4().as_bytes().to_vec();
    let waku_actor = WakuActor::new(Arc::new(node));
    let actor_ref = kameo::spawn(waku_actor);

    let actor_a = ActorA::new();
    let actor_a_ref = kameo::spawn(actor_a);

    pubsub.subscribe(actor_a_ref);

    let content_topics = Arc::new(Mutex::new(build_content_topics(&group_name, GROUP_VERSION)));

    assert!(actor_ref
        .ask(ProcessSubscribeToGroup {
            group_name: group_name.clone(),
        })
        .await
        .is_ok());

    waku_set_event_callback(move |signal| {
        match signal.event() {
            waku_bindings::Event::WakuMessage(event) => {
                let content_topic = event.waku_message().content_topic();
                // Check if message belongs to a relevant topic
                assert!(match_content_topic(&content_topics, content_topic));
                let msg = event.waku_message().clone();
                println!("msg: {:?}", msg.timestamp());
                assert!(sender_alice.blocking_send(msg).is_ok());
            }

            waku_bindings::Event::Unrecognized(data) => {
                error!("Unrecognized event!\n {data:?}");
            }
            _ => {
                error!(
                    "Unrecognized signal!\n {:?}",
                    serde_json::to_string(&signal)
                );
            }
        }
    });

    let sender = tokio::spawn(async move {
        for i in 0..10 {
            assert!(actor_ref
                .ask(ProcessMessageToSend {
                    msg: format!("test_message_{}", i).as_bytes().to_vec(),
                    subtopic: APP_MSG_SUBTOPIC.to_string(),
                    group_id: group_name.clone(),
                    app_id: uuid.clone(),
                })
                .await
                .is_ok());

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        info!("sender handle is finished");
    });

    let receiver = tokio::spawn(async move {
        while let Some(msg) = receiver_alice.recv().await {
            println!("msg received: {:?}", msg.timestamp());
            pubsub.publish(msg).await;
        }
        info!("receiver handle is finished");
    });

    tokio::select! {
        x = sender => {
            println!("get from sender: {:?}", x);
        }
        w = receiver => {
            println!("get from receiver: {:?}", w);
        }
    }
}
