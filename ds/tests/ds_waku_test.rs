use ds::{
    ds_waku::{build_content_topics, APP_MSG_SUBTOPIC},
    waku_actor::{ProcessMessageToSend, WakuNode},
    DeliveryServiceError,
};
use kameo::{
    actor::pubsub::PubSub,
    message::{Context, Message},
    Actor,
};
use log::info;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::channel;
use waku_bindings::WakuMessage;

#[derive(Debug, Clone, Actor)]
pub struct Application {
    pub app_id: String,
}

impl Application {
    pub fn new() -> Self {
        let app_id = uuid::Uuid::new_v4().to_string();
        Self { app_id }
    }
}

impl Message<WakuMessage> for Application {
    type Reply = Result<WakuMessage, DeliveryServiceError>;

    async fn handle(
        &mut self,
        msg: WakuMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        info!("Application received message: {:?}", msg.timestamp);
        Ok(msg)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_waku_client() {
    env_logger::init();
    let group_name = "new_group".to_string();
    let mut pubsub = PubSub::<WakuMessage>::new();

    let (sender, _) = channel::<WakuMessage>(100);
    let waku_node_default = WakuNode::new(60002)
        .await
        .expect("Failed to create WakuNode");

    let (sender_alice, mut receiver_alice) = channel::<WakuMessage>(100);
    let waku_node_init = WakuNode::new(60001)
        .await
        .expect("Failed to create WakuNode");

    let uuid = uuid::Uuid::new_v4().as_bytes().to_vec();
    let actor_a = Application::new();
    let actor_a_ref = kameo::spawn(actor_a);
    pubsub.subscribe(actor_a_ref);

    let content_topics = Arc::new(Mutex::new(build_content_topics(&group_name)));

    let waku_node_default = waku_node_default
        .start(sender, content_topics.clone())
        .await
        .expect("Failed to start WakuNode");

    let node_name = waku_node_default
        .listen_addresses()
        .await
        .expect("Failed to get listen addresses");
    let waku_node = waku_node_init
        .start(sender_alice, content_topics)
        .await
        .expect("Failed to start WakuNode");

    waku_node
        .connect_to_peers(node_name)
        .await
        .expect("Failed to connect to peers");

    tokio::spawn(async move {
        while let Some(msg) = receiver_alice.recv().await {
            info!("msg received from receiver_alice: {:?}", msg.timestamp);
            pubsub.publish(msg).await;
        }
        info!("receiver handle is finished");
    });

    tokio::task::block_in_place(move || {
        tokio::runtime::Handle::current().block_on(async move {
            let res = waku_node
                .send_message(ProcessMessageToSend {
                    msg: format!("test_message_1").as_bytes().to_vec(),
                    subtopic: APP_MSG_SUBTOPIC.to_string(),
                    group_id: group_name.clone(),
                    app_id: uuid.clone(),
                })
                .await;
            info!("res: {:?}", res);
            info!("sender handle is finished");
        });
    });
}
