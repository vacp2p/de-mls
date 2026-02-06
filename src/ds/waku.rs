//! Waku transport implementation and Waku-backed `DeliveryService`.

use std::{borrow::Cow, thread::sleep, time::Duration};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, error, info};
use waku_bindings::{
    node::PubsubTopic,
    node::{WakuNodeConfig, WakuNodeHandle},
    waku_new, Encoding, Initialized, LibwakuResponse, Multiaddr, Running, WakuContentTopic,
    WakuEvent, WakuMessage,
};

use crate::ds::{
    transport::{DeliveryService, InboundPacket, OutboundPacket},
    DeliveryServiceError,
};

pub const GROUP_VERSION: &str = "1";
pub const APP_MSG_SUBTOPIC: &str = "app_msg";
pub const WELCOME_SUBTOPIC: &str = "welcome";
pub const SUBTOPICS: [&str; 2] = [APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC];

/// The pubsub topic for the Waku Node.
/// Fixed for now because nodes on the network would need to be subscribed to existing pubsub topics.
pub fn pubsub_topic() -> PubsubTopic {
    PubsubTopic::new("/waku/2/rs/15/1")
}

/// Build the content topics for a group. Subtopics are fixed for de-mls group communication.
pub fn build_content_topics(group_name: &str) -> Vec<WakuContentTopic> {
    SUBTOPICS
        .iter()
        .map(|subtopic| build_content_topic(group_name, GROUP_VERSION, subtopic))
        .collect::<Vec<WakuContentTopic>>()
}

/// Build the content topic for the given group and subtopic.
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

impl From<WakuMessage> for InboundPacket {
    fn from(msg: WakuMessage) -> Self {
        InboundPacket {
            payload: msg.payload().to_vec(),
            subtopic: msg.content_topic.content_topic_name.to_string(),
            group_id: msg.content_topic.application_name.to_string(),
            app_id: msg.meta.clone(),
            timestamp: msg.timestamp as i64,
        }
    }
}

impl From<OutboundPacket> for WakuMessage {
    fn from(value: OutboundPacket) -> Self {
        WakuMessage::new(
            value.payload,
            build_content_topic(&value.group_id, GROUP_VERSION, &value.subtopic),
            2,
            value.app_id,
            true,
        )
    }
}

pub struct WakuNode<State> {
    node: WakuNodeHandle<State>,
}

impl WakuNode<Initialized> {
    /// Create a new WakuNode (initialized but not started).
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

    pub async fn start_with_handler<F>(
        self,
        on_message: F,
    ) -> Result<WakuNode<Running>, DeliveryServiceError>
    where
        F: Fn(WakuMessage) + Send + Sync + 'static,
    {
        let closure = move |response| {
            if let LibwakuResponse::Success(v) = response {
                let event: WakuEvent =
                    serde_json::from_str(v.unwrap().as_str()).expect("Parsing event to succeed");
                match event {
                    WakuEvent::WakuMessage(evt) => {
                        debug!("WakuMessage event received: {:?}", evt.message_hash);
                        on_message(evt.waku_message.clone());
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
    pub async fn send_message(&self, msg: OutboundPacket) -> Result<String, DeliveryServiceError> {
        let waku_message = msg.into();
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
}

#[derive(Debug)]
struct OutboundCommand {
    pkt: OutboundPacket,
    reply: oneshot::Sender<Result<String, DeliveryServiceError>>,
}

/// Waku-backed delivery service.
///
/// The service starts an embedded Waku node on its own dedicated thread/runtime.
#[derive(Clone)]
pub struct WakuDeliveryService {
    outbound: mpsc::Sender<OutboundCommand>,
    inbound: broadcast::Sender<InboundPacket>,
}

#[derive(Debug, Clone)]
pub struct WakuConfig {
    pub node_port: u16,
    /// Peer multiaddrs as strings.
    pub peers: Vec<String>,
}

impl WakuDeliveryService {
    fn parse_peers(peers: Vec<String>) -> Result<Vec<Multiaddr>, DeliveryServiceError> {
        peers
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| {
                s.parse::<Multiaddr>()
                    .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))
            })
            .collect()
    }

    pub async fn start(cfg: WakuConfig) -> Result<Self, DeliveryServiceError> {
        let (inbound_tx, _) = broadcast::channel::<InboundPacket>(256);
        let (out_tx, mut out_rx) = mpsc::channel::<OutboundCommand>(256);

        // Ensure callers can await startup success/failure.
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), DeliveryServiceError>>();

        let inbound_tx_thread = inbound_tx.clone();
        let peers = Self::parse_peers(cfg.peers)?;
        let node_port = cfg.node_port;

        std::thread::Builder::new()
            .name("waku-node".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("waku tokio runtime");

                rt.block_on(async move {
                    let started = async {
                        info!("Initializing waku node");
                        let waku_node_init = WakuNode::new(node_port as usize).await?;

                        let inbound_tx_cb = inbound_tx_thread.clone();
                        let waku_node = waku_node_init
                            .start_with_handler(move |waku_msg| {
                                let _ = inbound_tx_cb.send(waku_msg.into());
                            })
                            .await?;
                        info!("Waku node started");

                        if !peers.is_empty() {
                            waku_node.connect_to_peers(peers).await?;
                            info!("Connected to all peers");
                        }

                        Ok::<WakuNode<waku_bindings::Running>, DeliveryServiceError>(waku_node)
                    }
                    .await;

                    let waku_node = match started {
                        Ok(node) => {
                            let _ = ready_tx.send(Ok(()));
                            node
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return;
                        }
                    };

                    while let Some(cmd) = out_rx.recv().await {
                        let res = waku_node.send_message(cmd.pkt).await;
                        let _ = cmd.reply.send(res);
                    }

                    info!("Waku outbound loop finished");
                });
            })
            .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))?;

        // Wait for the node to either start or fail.
        ready_rx
            .await
            .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))??;

        Ok(Self {
            outbound: out_tx,
            inbound: inbound_tx,
        })
    }
}

impl DeliveryService for WakuDeliveryService {
    async fn send(&self, pkt: OutboundPacket) -> Result<String, DeliveryServiceError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.outbound
            .send(OutboundCommand {
                pkt,
                reply: reply_tx,
            })
            .await
            .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))?;

        reply_rx
            .await
            .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))?
    }

    fn subscribe(&self) -> broadcast::Receiver<InboundPacket> {
        self.inbound.subscribe()
    }
}
