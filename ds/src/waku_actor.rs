use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex as SyncMutex},
    thread::sleep,
    time::Duration,
};
use tokio::sync::mpsc::Sender;
use waku_bindings::{
    node::{WakuNodeConfig, WakuNodeHandle},
    waku_new, Initialized, LibwakuResponse, Multiaddr, Running, WakuContentTopic, WakuEvent,
    WakuMessage,
};

use crate::ds_waku::{pubsub_topic, GROUP_VERSION};
use crate::{ds_waku::build_content_topic, DeliveryServiceError};

pub struct WakuNode<State> {
    node: WakuNodeHandle<State>,
}

impl WakuNode<Initialized> {
    /// Create a new WakuNode
    /// Input:
    /// - node: The Waku Node to handle. Waku Node is already running
    pub async fn new(port: usize) -> Result<WakuNode<Initialized>, DeliveryServiceError> {
        let waku = waku_new(Some(WakuNodeConfig {
            tcp_port: Some(port),
            cluster_id: Some(15),
            shards: vec![1],
            log_level: Some("INFO"), // Supported: TRACE, DEBUG, INFO, NOTICE, WARN, ERROR or FATAL
            ..Default::default()
        }))
        .await
        .map_err(|e| DeliveryServiceError::WakuNodeAlreadyInitialized(e.to_string()))?;

        Ok(WakuNode { node: waku })
    }

    pub async fn start(
        self,
        waku_sender: Sender<WakuMessage>,
        content_topics: Arc<SyncMutex<Vec<WakuContentTopic>>>,
    ) -> Result<WakuNode<Running>, DeliveryServiceError> {
        let closure = move |response| {
            if let LibwakuResponse::Success(v) = response {
                let event: WakuEvent =
                    serde_json::from_str(v.unwrap().as_str()).expect("Parsing event to succeed");

                match event {
                    WakuEvent::WakuMessage(evt) => {
                        info!("WakuMessage event received: {:?}", evt.waku_message);
                        let content_topic = evt.waku_message.content_topic.clone();
                        // Check if message belongs to a relevant topic
                        if !match_content_topic(&content_topics, &content_topic) {
                            error!("Content topic not match: {:?}", content_topic);
                            return;
                        };
                        info!("Received message from waku: {:?}", evt.message_hash);
                        waku_sender
                            .blocking_send(evt.waku_message.clone())
                            .expect("Failed to send message to waku");
                    }
                    WakuEvent::RelayTopicHealthChange(_evt) => {
                        // dbg!("Relay topic change evt", evt);
                    }
                    WakuEvent::ConnectionChange(_evt) => {
                        // dbg!("Conn change evt", evt);
                    }
                    WakuEvent::Unrecognized(err) => panic!("Unrecognized waku event: {:?}", err),
                    _ => panic!("event case not expected"),
                };
            }
        };

        self.node
            .set_event_callback(closure)
            .expect("set event call back working");

        let waku = self.node.start().await.map_err(|e| {
            debug!("Failed to start the Waku Node: {:?}", e);
            DeliveryServiceError::WakuNodeAlreadyInitialized(e.to_string())
        })?;

        sleep(Duration::from_secs(2));

        waku.relay_subscribe(&pubsub_topic()).await.map_err(|e| {
            debug!("Failed to subscribe to the Waku Node: {:?}", e);
            DeliveryServiceError::WakuSubscribeToGroupError(e)
        })?;

        Ok(WakuNode { node: waku })
    }
}

impl WakuNode<Running> {
    pub async fn send_message(
        &self,
        msg: ProcessMessageToSend,
    ) -> Result<String, DeliveryServiceError> {
        let waku_message = msg.build_waku_message()?;
        let msg_id = self
            .node
            .relay_publish_message(&waku_message, &pubsub_topic(), None)
            .await
            .map_err(|e| {
                debug!("Failed to relay publish the message: {:?}", e);
                DeliveryServiceError::WakuPublishMessageError(e)
            })?;

        Ok(msg_id.to_string())
    }

    pub async fn connect_to_peers(
        &self,
        peer_addresses: Vec<Multiaddr>,
    ) -> Result<(), DeliveryServiceError> {
        for peer_address in peer_addresses {
            self.node
                .connect(&peer_address, None)
                .await
                .map_err(|e| DeliveryServiceError::WakuConnectPeerError(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn listen_addresses(&self) -> Result<Vec<Multiaddr>, DeliveryServiceError> {
        let addresses = self.node.listen_addresses().await.map_err(|e| {
            debug!("Failed to get the listen addresses: {:?}", e);
            DeliveryServiceError::WakuGetListenAddressesError(e)
        })?;

        Ok(addresses)
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
            self.app_id.clone(),
            true,
        ))
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
