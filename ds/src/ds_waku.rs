use core::result::Result;
use lazy_static::lazy_static;
use std::{
    borrow::Cow,
    error::Error,
    str::FromStr,
    sync::{Arc, Mutex as SyncMutex},
    thread,
    time::Duration,
};
use waku_bindings::*;

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

#[allow(clippy::field_reassign_with_default)]
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

/// Check if a content topic exists in a list of topics or if the list is empty
pub fn match_content_topic(
    content_topics: &Arc<SyncMutex<Vec<WakuContentTopic>>>,
    topic: &WakuContentTopic,
) -> bool {
    let locked_topics = content_topics.lock().unwrap();
    locked_topics.is_empty() || locked_topics.iter().any(|t| t == topic)
}
