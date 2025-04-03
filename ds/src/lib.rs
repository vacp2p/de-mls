use std::borrow::Cow;
use waku_bindings::{node::PubsubTopic, Encoding, WakuContentTopic};

pub mod waku_actor;

pub const GROUP_VERSION: &str = "1";
pub const APP_MSG_SUBTOPIC: &str = "app_msg";
pub const WELCOME_SUBTOPIC: &str = "welcome";
pub const SUBTOPICS: [&str; 2] = [APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC];

/// The pubsub topic for the Waku Node
/// Fixed for now because nodes on the network would need to be subscribed to existing pubsub topics
pub fn pubsub_topic() -> PubsubTopic {
    PubsubTopic::new("/waku/2/rs/15/1")
}

/// Build the content topics for a group. Subtopics are fixed for de-mls group communication.
///
/// Input:
/// - group_name: The name of the group
///
/// Returns:
/// - content_topics: The content topics of the group
pub fn build_content_topics(group_name: &str) -> Vec<WakuContentTopic> {
    SUBTOPICS
        .iter()
        .map(|subtopic| build_content_topic(group_name, GROUP_VERSION, subtopic))
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

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Waku publish message error: {0}")]
    WakuPublishMessageError(String),
    #[error("Waku subscribe to pubsub topic error: {0}")]
    WakuSubscribeToPubsubTopicError(String),
    #[error("Waku node already initialized: {0}")]
    WakuNodeAlreadyInitialized(String),
    #[error("Waku connect peer error: {0}")]
    WakuConnectPeerError(String),
    #[error("Waku get listen addresses error: {0}")]
    WakuGetListenAddressesError(String),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
