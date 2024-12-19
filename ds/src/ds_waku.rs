use core::result::Result;
use lazy_static::lazy_static;
use log::{error, trace};
use std::{
    borrow::Cow,
    error::Error,
    str::FromStr,
    sync::{Arc, Mutex as SyncMutex},
    thread,
    time::Duration,
};
use tokio::sync::mpsc::Sender;
use waku_bindings::*;

use crate::DeliveryServiceError;

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

pub fn pubsub_topic() -> WakuPubSubTopic {
    "/waku/2/rs/15/0".to_string()
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

pub fn setup_node_handle(nodes: Vec<String>) -> Result<WakuNodeHandle<Running>, Box<dyn Error>> {
    let mut config = WakuNodeConfig::default();
    config.port = Some(0);
    config.log_level = Some(WakuLogLevel::Panic);
    let node_handle = waku_new(Some(config))?;
    let node_handle = node_handle.start()?;
    let content_filter = ContentFilter::new(Some(pubsub_topic()), vec![]);
    node_handle.relay_subscribe(&content_filter)?;
    for address in nodes
        .iter()
        .map(|a| Multiaddr::from_str(a.as_str()).unwrap())
    {
        let peerid = node_handle.add_peer(&address, ProtocolId::Relay)?;
        node_handle.connect_peer_with_id(&peerid, None)?;
        thread::sleep(Duration::from_secs(2));
    }

    Ok(node_handle)
}

/// Parse and validate incoming message
pub fn handle_signal(
    signal: Signal,
    content_topics: &Arc<SyncMutex<Vec<WakuContentTopic>>>,
    app_id: Vec<u8>,
) -> Result<Option<WakuMessage>, DeliveryServiceError> {
    // Do not accept messages that were already received or sent by self
    match signal.event() {
        waku_bindings::Event::WakuMessage(event) => {
            let msg_app_id = event.waku_message().meta();
            // if msg_app_id == app_id {
            //     return Ok(None);
            // };
            let content_topic = event.waku_message().content_topic();
            // Check if message belongs to a relevant topic
            if !match_content_topic(content_topics, content_topic) {
                println!("Content topic not match: {:?}", content_topic);
                return Err(DeliveryServiceError::WakuInvalidContentTopic(format!(
                    "Skip irrelevant content topic: {:#?}",
                    content_topic
                )));
            };
            Ok(Some(event.waku_message().clone()))
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
    app_id: Vec<u8>,
    content_topics: Arc<SyncMutex<Vec<WakuContentTopic>>>,
) -> Result<(), DeliveryServiceError> {
    let handle_async = move |signal: Signal| {
        let msg = handle_signal(signal, &content_topics, app_id.clone());
        match msg {
            Ok(Some(m)) => match sender.blocking_send(m) {
                Ok(_) => trace!("Sent received message"),
                Err(e) => {
                    error!("Could not send message: {:#?}", e);
                }
            },
            Ok(None) => (),
            Err(e) => {
                error!("Could not handle message: {:#?}", e);
            }
        };
    };
    waku_set_event_callback(handle_async);
    Ok(())
}
