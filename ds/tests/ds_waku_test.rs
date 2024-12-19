use kameo::{
    actor::pubsub::PubSub,
    message::{Context, Message},
    Actor,
};
use log::error;
use std::{
    collections::HashSet, sync::{Arc, Mutex}, time::Duration
};
use tokio::sync::mpsc::channel;
use waku_bindings::WakuMessage;

use ds::{
    ds_waku::{
        build_content_topics, match_content_topic, register_handler, setup_node_handle,
        APP_MSG_SUBTOPIC, GROUP_VERSION, SUBTOPICS,
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

    let (sender_alice, mut receiver_alice) = channel(100);
    let node = setup_node_handle(vec![
        "/ip4/143.110.189.47/tcp/60000/p2p/16Uiu2HAkv6QDvJcQXKdsJKpJED5Ydh7cb2CZtRkPsjUhkg37ifP5"
            .to_string(),
    ])
    .unwrap();
    let uuid = uuid::Uuid::new_v4().as_bytes().to_vec();
    let waku_actor = WakuActor::new(Arc::new(node), uuid);
    let app_id = waku_actor.app_id();
    let actor_ref = kameo::spawn(waku_actor);

    let actor_a = ActorA::new();
    let actor_a_ref = kameo::spawn(actor_a);

    pubsub.subscribe(actor_a_ref);

    let content_topics = Arc::new(Mutex::new(build_content_topics(
        &group_name,
        GROUP_VERSION,
        &SUBTOPICS.clone(),
    )));

    let res = actor_ref
        .ask(ProcessSubscribeToGroup {
            group_name: group_name.clone(),
        })
        .await;
    println!("res: {:?}", res);

    waku_set_event_callback(move |signal| {
        match signal.event() {
            waku_bindings::Event::WakuMessage(event) => {
                let msg_app_id = event.waku_message().meta();
                // if msg_app_id == app_id {
                //     return Ok(None);
                // };
                let content_topic = event.waku_message().content_topic();
                // Check if message belongs to a relevant topic
                if !match_content_topic(&content_topics, content_topic) {
                    error!("Content topic not match: {:?}", content_topic);
                };
                let msg = event.waku_message().clone();
                println!("msg: {:?}", msg.timestamp());
                sender_alice.blocking_send(msg).unwrap();
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

    let handle2 = tokio::spawn(async move {
        for i in 0..10 {
            let res = actor_ref
                .ask(ProcessMessageToSend {
                    msg: format!("test_message").as_bytes().to_vec(),
                    subtopic: APP_MSG_SUBTOPIC.to_string(),
                    group_id: group_name.clone(),
                })
                .await;
            println!("res: {:?}", res);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        println!("handle2 finished");
    });

    let handle = tokio::spawn(async move {
        while let Some(msg) = receiver_alice.recv().await {
            println!("msg received: {:?}", msg.timestamp());
            pubsub.publish(msg).await;
        }
        println!("handle finished");
    });

    tokio::select! {
        x = handle2 => {
            println!("2res: {:?}", x);
        }
        w = handle => {
            println!("3msg received: {:?}", w);
        }
    }

    tokio::time::sleep(Duration::from_secs(10)).await;
}
