use chrono::Utc;
use core::result::Result;
use kameo::{
    message::{Context, Message},
    Actor,
};
use log::debug;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use waku_bindings::{Running, WakuContentTopic, WakuMessage, WakuNodeHandle};

use crate::ds_waku::{pubsub_topic, GROUP_VERSION};
use crate::{
    ds_waku::{build_content_topic, build_content_topics, content_filter},
    DeliveryServiceError,
};

/// WakuActor is the actor that handles the Waku Node
#[derive(Actor)]
pub struct WakuActor {
    node: Arc<WakuNodeHandle<Running>>,
}

impl WakuActor {
    /// Create a new WakuActor
    /// Input:
    /// - node: The Waku Node to handle. Waku Node is already running
    pub fn new(node: Arc<WakuNodeHandle<Running>>) -> Self {
        Self { node }
    }
}

/// Message to send to the Waku Node
/// This message is used to send a message to the Waku Node
/// Input:
/// - msg: The message to send
/// - subtopic: The subtopic to send the message to
/// - group_id: The group to send the message to
/// - app_id: The app is unique identifier for the application that is sending the message for filtering own messages
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessMessageToSend {
    pub msg: Vec<u8>,
    pub subtopic: String,
    pub group_id: String,
    pub app_id: Vec<u8>,
}

impl ProcessMessageToSend {
    /// Build a WakuMessage from the message to send
    /// Input:
    /// - msg: The message to send
    ///
    /// Returns:
    /// - WakuMessage: The WakuMessage to send
    pub fn build_waku_message(&self) -> Result<WakuMessage, DeliveryServiceError> {
        let content_topic = build_content_topic(&self.group_id, GROUP_VERSION, &self.subtopic);
        Ok(WakuMessage::new(
            self.msg.clone(),
            content_topic,
            2,
            Utc::now().timestamp() as usize,
            self.app_id.clone(),
            true,
        ))
    }
}

/// Handle the message to send to the Waku Node
/// Input:
/// - msg: The message to send
///
/// Returns:
/// - msg_id: The message id of the message sent to the Waku Node
impl Message<ProcessMessageToSend> for WakuActor {
    type Reply = Result<String, DeliveryServiceError>;

    async fn handle(
        &mut self,
        msg: ProcessMessageToSend,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let waku_message = msg.build_waku_message()?;
        let msg_id = self
            .node
            .relay_publish_message(&waku_message, Some(pubsub_topic()), None)
            .map_err(|e| {
                debug!("Failed to relay publish the message: {:?}", e);
                DeliveryServiceError::WakuPublishMessageError(e)
            })?;
        Ok(msg_id)
    }
}

/// Message for actor to subscribe to a group
/// It contains the group name to subscribe to
pub struct ProcessSubscribeToGroup {
    pub group_name: String,
}

/// Handle the message for actor to subscribe to a group
/// Input:
/// - group_name: The group to subscribe to
///
/// Returns:
/// - content_topics: The content topics of the group
impl Message<ProcessSubscribeToGroup> for WakuActor {
    type Reply = Result<Vec<WakuContentTopic>, DeliveryServiceError>;

    async fn handle(
        &mut self,
        msg: ProcessSubscribeToGroup,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let content_topics = build_content_topics(&msg.group_name, GROUP_VERSION);
        let content_filter = content_filter(&pubsub_topic(), &content_topics);
        self.node.relay_subscribe(&content_filter).map_err(|e| {
            debug!("Failed to relay subscribe to the group: {:?}", e);
            DeliveryServiceError::WakuSubscribeToGroupError(e)
        })?;
        Ok(content_topics)
    }
}

/// Message for actor to unsubscribe from a group
/// It contains the group name to unsubscribe from
pub struct ProcessUnsubscribeFromGroup {
    pub group_name: String,
}

/// Handle the message for actor to unsubscribe from a group
/// Input:
/// - group_name: The group to unsubscribe from
///
/// Returns:
/// - ()
impl Message<ProcessUnsubscribeFromGroup> for WakuActor {
    type Reply = Result<(), DeliveryServiceError>;

    async fn handle(
        &mut self,
        msg: ProcessUnsubscribeFromGroup,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let content_topics = build_content_topics(&msg.group_name, GROUP_VERSION);
        let content_filter = content_filter(&pubsub_topic(), &content_topics);
        self.node
            .relay_unsubscribe(&content_filter)
            .map_err(|e| DeliveryServiceError::WakuRelayTopicsError(e.to_string()))?;
        Ok(())
    }
}
