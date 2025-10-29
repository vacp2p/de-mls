use serde::{Deserialize, Serialize};
use std::{thread::sleep, time::Duration};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error, info};
use waku_bindings::{
    node::{WakuNodeConfig, WakuNodeHandle},
    waku_new, Initialized, LibwakuResponse, Multiaddr, Running, WakuEvent, WakuMessage,
};

use crate::{build_content_topic, pubsub_topic, DeliveryServiceError, GROUP_VERSION};

pub struct WakuNode<State> {
    node: WakuNodeHandle<State>,
}

impl WakuNode<Initialized> {
    /// Create a new WakuNode
    /// Input:
    /// - node: The Waku Node to handle. Waku Node is already running
    pub async fn new(port: usize) -> Result<WakuNode<Initialized>, DeliveryServiceError> {
        info!("Initializing waku node inside ");
        // Note: here we are auto-subscribing to the pubsub topic /waku/2/rs/15/1
        let waku = waku_new(Some(WakuNodeConfig {
            tcp_port: Some(port),
            cluster_id: Some(15),
            shards: vec![1],
            log_level: Some("FATAL"), // Supported: TRACE, DEBUG, INFO, NOTICE, WARN, ERROR or FATAL
            ..Default::default()
        }))
        .await
        .map_err(|e| DeliveryServiceError::WakuNodeAlreadyInitialized(e.to_string()))?;
        info!("Waku node initialized");
        Ok(WakuNode { node: waku })
    }

    pub async fn start(
        self,
        waku_sender: Sender<WakuMessage>,
    ) -> Result<WakuNode<Running>, DeliveryServiceError> {
        let closure = move |response| {
            if let LibwakuResponse::Success(v) = response {
                let event: WakuEvent =
                    serde_json::from_str(v.unwrap().as_str()).expect("Parsing event to succeed");
                match event {
                    WakuEvent::WakuMessage(evt) => {
                        debug!("WakuMessage event received: {:?}", evt.message_hash);
                        waku_sender
                            .blocking_send(evt.waku_message.clone())
                            .expect("Failed to send message to waku");
                    }
                    WakuEvent::RelayTopicHealthChange(evt) => {
                        debug!("Relay topic change evt: {evt:?}");
                    }
                    WakuEvent::ConnectionChange(evt) => {
                        debug!("Conn change evt: {evt:?}");
                    }
                    WakuEvent::Unrecognized(e) => panic!("Unrecognized waku event: {e:?}"),
                    _ => panic!("event case not expected"),
                };
            }
        };

        self.node
            .set_event_callback(closure)
            .expect("set event call back working");

        let waku = self.node.start().await.map_err(|e| {
            debug!("Failed to start the Waku Node: {e:?}");
            DeliveryServiceError::WakuNodeAlreadyInitialized(e.to_string())
        })?;

        sleep(Duration::from_secs(2));

        // Note: we are not subscribing to the pubsub topic here because we are already subscribed to it
        // and from waku side we can't check if we are subscribed to it or not
        // issue - https://github.com/waku-org/nwaku/issues/3246
        // waku.relay_subscribe(&pubsub_topic()).await.map_err(|e| {
        //     debug!("Failed to subscribe to the Waku Node: {:?}", e);
        //     DeliveryServiceError::WakuSubscribeToPubsubTopicError(e)
        // })?;

        Ok(WakuNode { node: waku })
    }
}

impl WakuNode<Running> {
    pub async fn send_message(
        &self,
        msg: WakuMessageToSend,
    ) -> Result<String, DeliveryServiceError> {
        let waku_message = msg.build_waku_message()?;
        let msg_id = self
            .node
            .relay_publish_message(&waku_message, &pubsub_topic(), None)
            .await
            .map_err(|e| {
                error!("Failed to relay publish the message: {e:?}");
                DeliveryServiceError::WakuPublishMessageError(e)
            })?;

        Ok(msg_id.to_string())
    }

    pub async fn connect_to_peers(
        &self,
        peer_addresses: Vec<Multiaddr>,
    ) -> Result<(), DeliveryServiceError> {
        for peer_address in peer_addresses {
            info!("Connecting to peer: {peer_address:?}");
            self.node
                .connect(&peer_address, Some(Duration::from_secs(10)))
                .await
                .map_err(|e| DeliveryServiceError::WakuConnectPeerError(e.to_string()))?;
            info!("Connected to peer: {peer_address:?}");
        }
        Ok(())
    }

    pub async fn listen_addresses(&self) -> Result<Vec<Multiaddr>, DeliveryServiceError> {
        let addresses = self.node.listen_addresses().await.map_err(|e| {
            debug!("Failed to get the listen addresses: {e:?}");
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
pub struct WakuMessageToSend {
    msg: Vec<u8>,
    subtopic: String,
    group_id: String,
    app_id: Vec<u8>,
}

impl WakuMessageToSend {
    /// Create a new WakuMessageToSend
    /// Input:
    /// - msg: The message to send
    /// - subtopic: The subtopic to send the message to
    /// - group_id: The group to send the message to
    /// - app_id: The app is unique identifier for the application that is sending the message for filtering own messages
    pub fn new(msg: Vec<u8>, subtopic: &str, group_id: &str, app_id: &[u8]) -> Self {
        Self {
            msg,
            subtopic: subtopic.to_string(),
            group_id: group_id.to_string(),
            app_id: app_id.to_vec(),
        }
    }
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

pub async fn run_waku_node(
    node_port: String,
    peer_addresses: Option<Vec<Multiaddr>>,
    waku_sender: Sender<WakuMessage>,
    receiver: &mut Receiver<WakuMessageToSend>,
) -> Result<(), DeliveryServiceError> {
    info!("Initializing waku node");
    let waku_node_init = WakuNode::new(
        node_port
            .parse::<usize>()
            .expect("Failed to parse node port"),
    )
    .await?;
    let waku_node = waku_node_init.start(waku_sender).await?;
    info!("Waku node started");

    if let Some(peer_addresses) = peer_addresses {
        waku_node.connect_to_peers(peer_addresses).await?;
        info!("Connected to all peers");
    }

    info!("Waiting for message to send to waku");
    while let Some(msg) = receiver.recv().await {
        debug!("Received message to send to waku");
        let id = waku_node.send_message(msg).await?;
        debug!("Successfully publish message with id: {id:?}");
    }

    Ok(())
}
