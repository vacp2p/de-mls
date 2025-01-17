use bounded_vec_deque::BoundedVecDeque;
use core::result::Result;
use log::{error, info};
use std::{
    borrow::Cow,
    str::FromStr,
    sync::{Arc, Mutex as SyncMutex},
    thread,
    time::Duration,
};
use tokio::sync::mpsc::Sender;
use waku_bindings::*;

use crate::DeliveryServiceError;

pub const GROUP_VERSION: &str = "1";
pub const APP_MSG_SUBTOPIC: &str = "app_msg";
pub const COMMIT_MSG_SUBTOPIC: &str = "commit_msg";
pub const WELCOME_SUBTOPIC: &str = "welcome";
pub const SUBTOPICS: [&str; 3] = [APP_MSG_SUBTOPIC, COMMIT_MSG_SUBTOPIC, WELCOME_SUBTOPIC];

/// The pubsub topic for the Waku Node
/// Fixed for now because nodes on the network would need to be subscribed to existing pubsub topics
pub fn pubsub_topic() -> WakuPubSubTopic {
    "/waku/2/rs/15/0".to_string()
}

/// Build the content topics for a group. Subtopics are fixed for de-mls group communication.
///
/// Input:
/// - group_name: The name of the group
/// - group_version: The version of the group
///
/// Returns:
/// - content_topics: The content topics of the group
pub fn build_content_topics(group_name: &str, group_version: &str) -> Vec<WakuContentTopic> {
    SUBTOPICS
        .iter()
        .map(|subtopic| build_content_topic(group_name, group_version, subtopic))
        .collect::<Vec<WakuContentTopic>>()
}

/// Build the content topic for the given group and subtopic
/// Input:
/// - group_name: The name of the group
/// - group_version: The version of the group
/// - subtopic: The subtopic of the group
///
/// Returns:
/// - content_topic: The content topic of the subtopic
pub fn build_content_topic(
    group_name: &str,
    group_version: &str,
    subtopic: &str,
) -> WakuContentTopic {
    WakuContentTopic {
        application_name: Cow::from(group_name.to_string()),
        version: Cow::from(group_version.to_string()),
        content_topic_name: Cow::from(subtopic.to_string()),
        encoding: Encoding::Proto,
    }
}

/// Build the content filter for the given pubsub topic and content topics
/// Input:
/// - pubsub_topic: The pubsub topic of the Waku Node
/// - content_topics: The content topics of the group
///
/// Returns:
/// - content_filter: The content filter of the group
pub fn content_filter(
    pubsub_topic: &WakuPubSubTopic,
    content_topics: &[WakuContentTopic],
) -> ContentFilter {
    ContentFilter::new(Some(pubsub_topic.to_string()), content_topics.to_vec())
}

/// Setup the Waku Node Handle
/// Input:
/// - nodes_addresses: The addresses of the nodes to connect to
///
/// Returns:
/// - node_handle: The Waku Node Handle
#[allow(clippy::field_reassign_with_default)]
pub fn setup_node_handle(
    nodes_addresses: Vec<String>,
) -> Result<WakuNodeHandle<Running>, DeliveryServiceError> {
    let mut config = WakuNodeConfig::default();
    // Set the port to 0 to let the system choose a random port
    config.port = Some(0);
    config.log_level = Some(WakuLogLevel::Panic);
    let node_handle = waku_new(Some(config))
        .map_err(|e| DeliveryServiceError::WakuNodeAlreadyInitialized(e.to_string()))?;
    let node_handle = node_handle
        .start()
        .map_err(|e| DeliveryServiceError::WakuNodeAlreadyInitialized(e.to_string()))?;
    let content_filter = ContentFilter::new(Some(pubsub_topic()), vec![]);
    node_handle
        .relay_subscribe(&content_filter)
        .map_err(|e| DeliveryServiceError::WakuSubscribeToContentFilterError(e.to_string()))?;
    for address in nodes_addresses
        .iter()
        .map(|a| Multiaddr::from_str(a.as_str()))
    {
        let address =
            address.map_err(|e| DeliveryServiceError::FailedToParseMultiaddr(e.to_string()))?;
        let peerid = node_handle
            .add_peer(&address, ProtocolId::Relay)
            .map_err(|e| DeliveryServiceError::WakuAddPeerError(e.to_string()))?;
        node_handle
            .connect_peer_with_id(&peerid, None)
            .map_err(|e| DeliveryServiceError::WakuConnectPeerError(e.to_string()))?;
        thread::sleep(Duration::from_secs(2));
    }

    Ok(node_handle)
}

/// Check if a content topic exists in a list of topics or if the list is empty
pub fn match_content_topic(
    content_topics: &Arc<SyncMutex<Vec<WakuContentTopic>>>,
    topic: &WakuContentTopic,
) -> bool {
    let locked_topics = content_topics.lock().unwrap();
    locked_topics.is_empty() || locked_topics.iter().any(|t| t == topic)
}

pub async fn handle_waku_event(
    waku_sender: Sender<WakuMessage>,
    content_topics: Arc<SyncMutex<Vec<WakuContentTopic>>>,
) {
    info!("Setting up waku event callback");
    let mut seen_messages = BoundedVecDeque::<String>::new(40);
    waku_set_event_callback(move |signal| {
        match signal.event() {
            waku_bindings::Event::WakuMessage(event) => {
                let msg_id = event.message_id();
                if seen_messages.contains(msg_id) {
                    return;
                }
                seen_messages.push_back(msg_id.clone());
                let content_topic = event.waku_message().content_topic();
                // Check if message belongs to a relevant topic
                if !match_content_topic(&content_topics, content_topic) {
                    error!("Content topic not match: {:?}", content_topic);
                    return;
                };
                let msg = event.waku_message().clone();
                info!("Received message from waku: {:?}", event.message_id());
                waku_sender
                    .blocking_send(msg)
                    .expect("Failed to send message to waku");
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
}
