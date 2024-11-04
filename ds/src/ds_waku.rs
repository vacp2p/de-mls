use chrono::Utc;
use std::sync::Mutex as SyncMutex;
use std::{borrow::Cow, collections::HashSet, str::FromStr, sync::Arc};

use core::result::Result;
use serde::{Deserialize, Serialize};
use std::error::Error;
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::{debug, error, trace};
use waku_bindings::*;

use crate::DeliveryServiceError;

pub fn pubsub_topic(group_name: &str) -> WakuPubSubTopic {
    "/waku/2/".to_string() + group_name + "/proto"
}

pub fn build_content_topics(
    group_name: &str,
    group_version: String,
    subtopics: &[String],
) -> Vec<WakuContentTopic> {
    (*subtopics
        .iter()
        .map(|subtopic| WakuContentTopic {
            application_name: Cow::from(group_name.to_string()),
            version: Cow::from(group_version.clone()),
            content_topic_name: Cow::from(subtopic.to_string()),
            encoding: Encoding::Proto,
        })
        .collect::<Vec<WakuContentTopic>>())
    .to_vec()
}

pub fn content_filter(
    pubsub_topic: &WakuPubSubTopic,
    content_topics: &[WakuContentTopic],
) -> ContentFilter {
    ContentFilter::new(Some(pubsub_topic.to_string()), content_topics.to_vec())
}

/// Subscribe to pubsub topic on the relay protocol
pub fn relay_subscribe(
    node_handle: &WakuNodeHandle<Running>,
    content_filter: &ContentFilter,
) -> Result<(), DeliveryServiceError> {
    node_handle
        .relay_subscribe(content_filter)
        .map_err(|e| DeliveryServiceError::WakuCreateNodeError(e.to_string()))
}

fn setup_node_handle(nodes: Vec<String>) -> Result<WakuNodeHandle<Running>, Box<dyn Error>> {
    let node_handle = waku_new(None)?;
    let node_handle = node_handle.start()?;
    for address in nodes
        .iter()
        .map(|a| Multiaddr::from_str(a.as_str()).unwrap())
    {
        let peerid = node_handle.add_peer(&address, ProtocolId::Relay)?;
        node_handle.connect_peer_with_id(&peerid, None)?;
    }

    Ok(node_handle)
}

/// Parse and validate incoming message
pub fn handle_signal(
    signal: Signal,
    seen_msg_ids: &Arc<SyncMutex<HashSet<String>>>,
    content_topics: &Arc<SyncMutex<Vec<WakuContentTopic>>>,
) -> Result<WakuMessage, DeliveryServiceError> {
    // Do not accept messages that were already received or sent by self
    match signal.event() {
        waku_bindings::Event::WakuMessage(event) => {
            let msg_id = event.message_id();
            let mut ids = seen_msg_ids.lock().unwrap();
            // Check if message has been received before or sent from local node
            if ids.contains(msg_id) {
                return Err(DeliveryServiceError::WakuInvalidMessage(format!(
                    "Skip repeated message: {:#?}",
                    msg_id
                )));
            };
            ids.insert(msg_id.to_string());
            let content_topic = event.waku_message().content_topic();
            // Check if message belongs to a relevant topic
            if !match_content_topic(content_topics, content_topic) {
                return Err(DeliveryServiceError::WakuInvalidMessage(format!(
                    "Skip irrelevant content topic: {:#?}",
                    content_topic
                )));
            };
            Ok(event.waku_message().clone())
        }

        waku_bindings::Event::Unrecognized(data) => Err(DeliveryServiceError::WakuInvalidMessage(
            format!("Unrecognized event!\n {data:?}"),
        )),
        _ => Err(DeliveryServiceError::WakuInvalidMessage(format!(
            "Unrecognized signal!\n {:?}",
            serde_json::to_string(&signal)
        ))),
    }
}

/// Check if a content topic exists in a list of topics or if the list is empty
pub fn match_content_topic(
    content_topics: &Arc<SyncMutex<Vec<WakuContentTopic>>>,
    topic: &WakuContentTopic,
) -> bool {
    let locked_topics = content_topics.lock().unwrap();
    locked_topics.is_empty() || locked_topics.iter().any(|t| t == topic)
}

pub fn register_handler(
    sender: Sender<WakuMessage>,
    seen_msg_ids: Arc<SyncMutex<HashSet<String>>>,
    content_topics: Arc<SyncMutex<Vec<WakuContentTopic>>>,
) -> Result<(), DeliveryServiceError> {
    let handle_async = move |signal: Signal| {
        let msg = handle_signal(signal, &seen_msg_ids, &content_topics);

        if let Ok(m) = msg {
            match sender.send(m) {
                Ok(_) => trace!("Sent received message to radio operator"),
                Err(e) => error!("Could not send message to channel: {:#?}", e),
            }
        }
    };

    waku_set_event_callback(handle_async);
    Ok(())
}

pub struct WakuClient {
    node: WakuNodeHandle<Running>,
    pub pubsub_topic: WakuPubSubTopic,
    pub content_topics: Arc<SyncMutex<Vec<WakuContentTopic>>>,
    /// A set of message ids sent from the group
    pub seen_msg_ids: Arc<SyncMutex<HashSet<String>>>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct DeMlsMesage {
    pub sender: String,
    pub msg: Vec<u8>,
}

impl WakuClient {
    pub fn new_with_group(
        nodes: Vec<String>,
        group_id: String,
        sender: Sender<WakuMessage>,
    ) -> Result<Self, DeliveryServiceError> {
        let node = setup_node_handle(nodes).unwrap();

        let pubsub_topic = pubsub_topic(&group_id);
        let content_topics = build_content_topics(&group_id, "1".to_string(), &["msg".to_string()]);
        let content_filter = content_filter(&pubsub_topic, &content_topics);
        node.relay_subscribe(&content_filter)
            .map_err(|e| DeliveryServiceError::WakuRelayError(e.to_string()))?;

        let seen_msg_ids = Arc::new(SyncMutex::new(HashSet::new()));
        let content_topics = Arc::new(SyncMutex::new(content_topics));
        register_handler(sender, seen_msg_ids.clone(), content_topics.clone())
            .expect("Could not register handler");

        Ok(WakuClient {
            node,
            pubsub_topic,
            content_topics,
            seen_msg_ids,
        })
    }

    pub async fn remove_group(&mut self, group_id: String) -> Result<(), DeliveryServiceError> {
        Ok(())
    }

    pub fn send_to_waku(
        &self,
        msg: Vec<u8>,
        pubsub_topic: WakuPubSubTopic,
        content_topic: WakuContentTopic,
    ) -> Result<String, DeliveryServiceError> {
        let waku_message = WakuMessage::new(
            msg,
            content_topic,
            2,
            Utc::now().timestamp() as usize,
            vec![],
            true,
        );

        self.node
            .relay_publish_message(&waku_message, Some(pubsub_topic.clone()), None)
            .map_err(|e| {
                debug!(
                    error = tracing::field::debug(&e),
                    "Failed to relay publish the message"
                );
                DeliveryServiceError::WakuPublishMessageError(e)
            })
    }

    pub fn stop(self) -> Result<(), DeliveryServiceError> {
        trace!("Set an empty event callback");
        waku_set_event_callback(|_| {});
        debug!("Stop Waku node");
        self.node
            .stop()
            .map_err(|e| DeliveryServiceError::WakuStopNodeError(e))?;
        trace!("Drop Arc std sync mutexes");
        drop(self.content_topics);
        drop(self.seen_msg_ids);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{env, sync::mpsc};

    use super::*;

    #[tokio::test]
    async fn test_waku_client() {
        let node = env::var("NODE").unwrap();
        debug!("Node: {}", node);

        let (sender, receiver) = tokio::sync::broadcast::channel::<WakuMessage>(16);
        let waku_client =
            WakuClient::new_with_group(vec![node], "test".to_string(), sender).unwrap();

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
}
