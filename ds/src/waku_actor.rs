use chrono::Utc;
use core::result::Result;
use kameo::{
    message::{Context, Message},
    Actor,
};
use log::debug;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Receiver;
use waku_bindings::{Running, WakuContentTopic, WakuMessage, WakuNodeHandle};

use crate::ds_waku::{pubsub_topic, GROUP_VERSION, SUBTOPICS};
use crate::{
    ds_waku::{build_content_topic, build_content_topics, content_filter},
    DeliveryServiceError,
};

#[derive(Actor)]
pub struct WakuActor {
    node: Arc<WakuNodeHandle<Running>>,
    app_id: Vec<u8>,
}

impl WakuActor {
    pub fn new(node: Arc<WakuNodeHandle<Running>>, app_id: Vec<u8>) -> Self {
        Self {
            node,
            app_id,
        }
    }

    pub fn app_id(&self) -> Vec<u8> {
        self.app_id.clone()
    }
}

// Messages
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessMessageToSend {
    pub msg: Vec<u8>,
    pub subtopic: String,
    pub group_id: String,
}

impl Message<ProcessMessageToSend> for WakuActor {
    type Reply = Result<String, DeliveryServiceError>;

    async fn handle(
        &mut self,
        msg: ProcessMessageToSend,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let waku_message = build_waku_message(msg, self.app_id.clone())?;
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

// pub struct ReceiveMessage {
// }

// impl Message<ReceiveMessage> for WakuActor {
//     type Reply = Result<WakuMessage, DeliveryServiceError>;

//     async fn handle(
//         &mut self,
//         msg: ReceiveMessage,
//         _ctx: Context<'_, Self, Self::Reply>,
//     ) -> Self::Reply {
        
//     }
// }

pub struct ProcessSubscribeToGroup {
    pub group_name: String,
}

impl Message<ProcessSubscribeToGroup> for WakuActor {
    type Reply = Result<Vec<WakuContentTopic>, DeliveryServiceError>;

    async fn handle(
        &mut self,
        msg: ProcessSubscribeToGroup,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let content_topics =
            build_content_topics(&msg.group_name, GROUP_VERSION, &SUBTOPICS.clone());
        let content_filter = content_filter(&pubsub_topic(), &content_topics);
        self.node.relay_subscribe(&content_filter).map_err(|e| {
            debug!("Failed to relay subscribe to the group: {:?}", e);
            DeliveryServiceError::WakuSubscribeToGroupError(e)
        })?;
        Ok(content_topics)
    }
}

pub struct ProcessUnsubscribeFromGroup {
    pub group_name: String,
}

impl Message<ProcessUnsubscribeFromGroup> for WakuActor {
    type Reply = Result<(), DeliveryServiceError>;

    async fn handle(
        &mut self,
        msg: ProcessUnsubscribeFromGroup,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let content_topics =
            build_content_topics(&msg.group_name, GROUP_VERSION, &SUBTOPICS.clone());
        let content_filter = content_filter(&pubsub_topic(), &content_topics);
        self.node
            .relay_unsubscribe(&content_filter)
            .map_err(|e| DeliveryServiceError::WakuRelayTopicsError(e.to_string()))?;
        Ok(())
    }
}

pub fn build_waku_message(
    msg: ProcessMessageToSend,
    app_id: Vec<u8>,
) -> Result<WakuMessage, DeliveryServiceError> {
    let content_topic = build_content_topic(&msg.group_id, GROUP_VERSION, &msg.subtopic);
    Ok(WakuMessage::new(
        msg.msg,
        content_topic,
        2,
        Utc::now().timestamp() as usize,
        app_id,
        true,
    ))
}
