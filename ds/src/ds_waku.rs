use chrono::Utc;
use core::result::Result;
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::sync::{mpsc::Sender, Arc, Mutex as SyncMutex};
use std::{borrow::Cow, collections::HashSet, str::FromStr};
use tracing::{debug, error, trace};
use waku_bindings::*;

use crate::DeliveryServiceError;

pub fn pubsub_topic(group_name: &str) -> WakuPubSubTopic {
    "/waku/2/".to_string() + group_name + "/proto"
}

pub fn build_content_topics(
    group_name: &str,
    group_version: &str,
    subtopics: &[&str],
) -> Vec<WakuContentTopic> {
    (*subtopics
        .iter()
        .map(|subtopic| WakuContentTopic {
            application_name: Cow::from(group_name.to_string()),
            version: Cow::from(group_version.to_string()),
            content_topic_name: Cow::from(subtopic.to_string()),
            encoding: Encoding::Proto,
        })
        .collect::<Vec<WakuContentTopic>>())
    .to_vec()
}

pub fn build_content_topic(
    group_name: &str,
    group_version: &str,
    subtopic: &str,
) -> WakuContentTopic {
    build_content_topics(group_name, group_version, &[subtopic])[0].clone()
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

pub fn setup_node_handle(nodes: Vec<String>) -> Result<WakuNodeHandle<Running>, Box<dyn Error>> {
    let mut config = WakuNodeConfig::default();
    config.log_level = Some(WakuLogLevel::Panic);
    let node_handle = waku_new(Some(config))?;
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
                println!("Message already received or sent from local node: {:?}", msg_id);
                return Err(DeliveryServiceError::WakuInvalidMessage(format!(
                    "Skip repeated message: {:#?}",
                    msg_id
                )));
            };
            println!("Received new message: {:?}", msg_id);
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
                Ok(_) => trace!("Sent received message"),
                Err(e) => error!("Could not send message: {:#?}", e),
            }
        }
    };

    waku_set_event_callback(handle_async);
    Ok(())
}

pub struct WakuGroupClient {
    pub group_id: String,
    pub pubsub_topic: WakuPubSubTopic,
    pub content_topics: Arc<SyncMutex<Vec<WakuContentTopic>>>,
    /// A set of message ids sent from the group
    pub seen_msg_ids: Arc<SyncMutex<HashSet<String>>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DeMlsMessage {
    pub sender: String,
    pub msg: Vec<u8>,
    pub msg_type: MsgType,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgType {
    Text,
    MlsText,
    UpdateGroup,
    InviteToGroup,
    RemoveFromGroup,
}

impl MsgType {
    pub fn to_string(&self) -> String {
        match self {
            MsgType::Text => "text".to_string(),
            MsgType::MlsText => "mls_text".to_string(),
            MsgType::UpdateGroup => "update_group".to_string(),
            MsgType::InviteToGroup => "invite_to_group".to_string(),
            MsgType::RemoveFromGroup => "remove_from_group".to_string(),
        }
    }
}

pub const TEST_GROUP_NAME: &str = "new_group";
pub const GROUP_VERSION: &str = "1";
pub const APP_MSG_SUBTOPIC: &str = "app_msg";
pub const COMMIT_MSG_SUBTOPIC: &str = "commit_msg";
pub const WELCOME_SUBTOPIC: &str = "welcome";
pub const SUBTOPICS: [&str; 3] = [APP_MSG_SUBTOPIC, COMMIT_MSG_SUBTOPIC, WELCOME_SUBTOPIC];

lazy_static! {
    pub static ref TEST_WELCOME_CONTENT_TOPIC: WakuContentTopic =
        build_content_topic(TEST_GROUP_NAME, GROUP_VERSION, WELCOME_SUBTOPIC);
    pub static ref TEST_APP_MSG_CONTENT_TOPIC: WakuContentTopic =
        build_content_topic(TEST_GROUP_NAME, GROUP_VERSION, APP_MSG_SUBTOPIC);
    pub static ref TEST_COMMIT_MSG_CONTENT_TOPIC: WakuContentTopic =
        build_content_topic(TEST_GROUP_NAME, GROUP_VERSION, COMMIT_MSG_SUBTOPIC);
}

impl WakuGroupClient {
    pub fn waku_relay_topics(&self, node: &WakuNodeHandle<Running>) -> Vec<String> {
        let topics = node.relay_topics().unwrap();
        debug!("topics: {:?}", topics);
        topics
    }

    pub fn new(
        node: &WakuNodeHandle<Running>,
        group_id: String,
        sender: Sender<WakuMessage>,
    ) -> Result<Self, DeliveryServiceError> {
        // check if already subscribed to the pubsub topic
        let pubsub_topic = pubsub_topic(&group_id);
        let topics = node.relay_topics();
        if let Ok(topics) = topics {
            if topics.contains(&pubsub_topic) {
                return Err(DeliveryServiceError::WakuAlreadySubscribed(format!(
                    "Already subscribed to the pubsub topic: {:#?}",
                    pubsub_topic
                )));
            }
        }

        let content_topics = build_content_topics(&group_id, GROUP_VERSION, &SUBTOPICS.clone());
        let content_filter = content_filter(&pubsub_topic, content_topics.as_ref());
        node.relay_subscribe(&content_filter)
            .map_err(|e| DeliveryServiceError::WakuRelayError(e.to_string()))?;

        let seen_msg_ids = Arc::new(SyncMutex::new(HashSet::new()));
        let content_topics = Arc::new(SyncMutex::new(content_topics));
        register_handler(sender, seen_msg_ids.clone(), content_topics.clone())
            .expect("Could not register handler");

        Ok(WakuGroupClient {
            group_id,
            pubsub_topic,
            content_topics,
            seen_msg_ids,
        })
    }

    pub async fn remove_group(
        &mut self,
        node: &WakuNodeHandle<Running>,
        group_id: String,
    ) -> Result<(), DeliveryServiceError> {
        Ok(())
    }

    pub fn send_to_waku(
        &self,
        node: &WakuNodeHandle<Running>,
        msg: Vec<u8>,
        msg_type: MsgType,
    ) -> Result<String, DeliveryServiceError> {
        let content_topic = match msg_type {
            MsgType::Text => build_content_topic(&self.group_id, GROUP_VERSION, WELCOME_SUBTOPIC),
            MsgType::MlsText => {
                build_content_topic(&self.group_id, GROUP_VERSION, APP_MSG_SUBTOPIC)
            }
            MsgType::InviteToGroup => {
                build_content_topic(&self.group_id, GROUP_VERSION, WELCOME_SUBTOPIC)
            }
            MsgType::RemoveFromGroup => {
                build_content_topic(&self.group_id, GROUP_VERSION, COMMIT_MSG_SUBTOPIC)
            }
            MsgType::UpdateGroup => {
                build_content_topic(&self.group_id, GROUP_VERSION, COMMIT_MSG_SUBTOPIC)
            }
        };

        let waku_message = WakuMessage::new(
            msg,
            content_topic,
            2,
            Utc::now().timestamp() as usize,
            vec![],
            true,
        );

        node.relay_publish_message(&waku_message, Some(self.pubsub_topic.clone()), None)
            .map_err(|e| {
                debug!(
                    error = tracing::field::debug(&e),
                    "Failed to relay publish the message"
                );
                DeliveryServiceError::WakuPublishMessageError(e)
            })
            .map(|id| {
                self.seen_msg_ids.lock().unwrap().insert(id.clone());
                trace!(id = id, "Sent message");
                id
            })
    }

    // pub fn stop(self, node: &WakuNodeHandle<Running>) -> Result<(), DeliveryServiceError> {
    //     trace!("Set an empty event callback");
    //     waku_set_event_callback(|_| {});
    //     debug!("Stop Waku node");
    //     node
    //         .stop()
    //         .map_err(|e| DeliveryServiceError::WakuStopNodeError(e))?;
    //     trace!("Drop Arc std sync mutexes");
    //     drop(self.content_topics);
    //     drop(self.seen_msg_ids);
    //     Ok(())
    // }
}
