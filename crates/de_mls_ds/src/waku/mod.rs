//! Waku transport implementation and Waku-backed `DeliveryService`.

pub(crate) mod sys;
pub(crate) mod wrapper;

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use tracing::{debug, error, info};

use crate::{
    CONVERSATION_VERSION, DeliveryServiceError, SUBTOPICS,
    transport::{DeliveryService, InboundPacket, OutboundPacket},
    waku::wrapper::WakuNodeCtx,
};

/// Pubsub topic of the Waku Node.
pub fn pubsub_topic() -> String {
    "/waku/2/rs/15/1".to_string()
}

/// Content topics this conversation subscribes to (one per subtopic).
pub fn build_content_topics(conversation_id: &str) -> Vec<String> {
    SUBTOPICS
        .iter()
        .map(|subtopic| build_content_topic(conversation_id, CONVERSATION_VERSION, subtopic))
        .collect()
}

/// `/{conversation_id}/{version}/{subtopic}/proto`.
pub fn build_content_topic(
    conversation_id: &str,
    conversation_version: &str,
    subtopic: &str,
) -> String {
    format!("/{conversation_id}/{conversation_version}/{subtopic}/proto")
}

struct OutboundCommand {
    pkt: OutboundPacket,
    reply: mpsc::SyncSender<Result<String, DeliveryServiceError>>,
}

type SubscriberList = Arc<Mutex<Vec<mpsc::SyncSender<InboundPacket>>>>;

/// Result returned by [`WakuDeliveryService::start`].
pub struct WakuStartResult {
    pub service: WakuDeliveryService,
    /// Local ENR — present when discv5 is enabled; pass to other nodes via
    /// [`WakuConfig::discv5_bootstrap_enrs`].
    pub enr: Option<String>,
}

/// Waku-backed delivery service. Runs an embedded Waku node on a
/// dedicated `std::thread`; all interaction is via synchronous
/// `std::sync::mpsc` channels. Drop every clone (or call
/// [`shutdown`](Self::shutdown)) to stop the node.
#[derive(Clone, Debug)]
pub struct WakuDeliveryService {
    outbound: mpsc::SyncSender<OutboundCommand>,
    subscribers: SubscriberList,
    enr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WakuConfig {
    pub node_port: u16,
    /// Enable discv5 peer discovery.
    pub discv5: bool,
    /// UDP port for discv5 discovery.
    pub discv5_udp_port: u16,
    /// Bootstrap ENRs for discv5. Empty means this node acts as a
    /// bootstrap node (peers must know its ENR out-of-band).
    pub discv5_bootstrap_enrs: Vec<String>,
}

impl Default for WakuConfig {
    fn default() -> Self {
        Self {
            node_port: 60000,
            discv5: false,
            discv5_udp_port: 9000,
            discv5_bootstrap_enrs: Vec::new(),
        }
    }
}

impl WakuDeliveryService {
    /// Start the node and return the delivery service + the local ENR
    /// (when discv5 is enabled).
    pub fn start(cfg: WakuConfig) -> Result<WakuStartResult, DeliveryServiceError> {
        let (out_tx, out_rx) = mpsc::sync_channel::<OutboundCommand>(256);
        let subscribers: SubscriberList = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<Option<String>, DeliveryServiceError>>();

        let subs_for_thread = subscribers.clone();

        thread::Builder::new()
            .name("waku-node".into())
            .spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    Self::node_thread(cfg, out_rx, subs_for_thread, ready_tx);
                })) {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    error!("waku-node thread panicked: {msg}");
                }
            })
            .map_err(DeliveryServiceError::ThreadSpawn)?;

        let enr = ready_rx
            .recv()
            .map_err(|_| DeliveryServiceError::WakuChannelClosed)??;

        let service = Self {
            outbound: out_tx,
            subscribers,
            enr: enr.clone(),
        };
        Ok(WakuStartResult { service, enr })
    }

    /// Local ENR — `Some` when discv5 is enabled.
    pub fn enr(&self) -> Option<&str> {
        self.enr.as_deref()
    }

    /// Shut down the background Waku node thread. Subsequent
    /// [`publish`](DeliveryService::publish) calls return an error.
    pub fn shutdown(self) {
        drop(self.outbound);
    }

    fn node_thread(
        cfg: WakuConfig,
        out_rx: mpsc::Receiver<OutboundCommand>,
        subscribers: SubscriberList,
        ready_tx: mpsc::Sender<Result<Option<String>, DeliveryServiceError>>,
    ) {
        let mut config = serde_json::json!({
            "host": "0.0.0.0",
            "port": cfg.node_port,
            "relay": true,
            "clusterId": 15,
            "shards": [1],
            "numShardsInNetwork": 8,
            "logLevel": "ERROR",
            // Disable libwaku metrics — would expose Prometheus/logging
            // endpoints from embedded nodes otherwise.
            "metricsServer": false,
            "metricsLogging": false,
        });

        if cfg.discv5 {
            config["discv5Discovery"] = serde_json::json!(true);
            config["discv5UdpPort"] = serde_json::json!(cfg.discv5_udp_port);
            if !cfg.discv5_bootstrap_enrs.is_empty() {
                config["discv5BootstrapNodes"] = serde_json::json!(cfg.discv5_bootstrap_enrs);
            }
        }

        let config_json = config.to_string();

        let waku = match WakuNodeCtx::new(&config_json) {
            Ok(w) => w,
            Err(e) => {
                let _ = ready_tx.send(Err(DeliveryServiceError::WakuStartup(e)));
                return;
            }
        };

        if let Err(e) = waku.start() {
            let _ = ready_tx.send(Err(DeliveryServiceError::WakuStartup(e)));
            return;
        }
        info!("Waku node started");

        thread::sleep(Duration::from_secs(2));

        // discv5 is auto-started by `waku_start` when `discv5Discovery=true`
        // is in the config JSON — we only fetch the ENR here.
        let local_enr = if cfg.discv5 {
            match waku.get_enr() {
                Ok(enr) => {
                    info!("Local ENR: {enr}");
                    Some(enr)
                }
                Err(e) => {
                    info!("Could not retrieve ENR: {e}");
                    None
                }
            }
        } else {
            None
        };

        // Explicit subscribe as a safety net — config shards may auto-
        // subscribe, but behaviour varies between libwaku versions.
        let topic = pubsub_topic();
        if let Err(e) = waku.relay_subscribe(&topic) {
            info!("relay_subscribe returned (may already be subscribed): {e}");
        }

        let subs_for_cb = subscribers.clone();
        let event_closure = move |_ret: i32, data: &str| {
            if let Some(pkt) = Self::parse_event(data) {
                let mut guard = match subs_for_cb.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        error!("Subscriber mutex poisoned, dropping event: {e}");
                        return;
                    }
                };
                guard.retain(|tx| match tx.try_send(pkt.clone()) {
                    Ok(()) => true,
                    Err(mpsc::TrySendError::Full(_)) => true,
                    Err(mpsc::TrySendError::Disconnected(_)) => false,
                });
            }
        };
        // Box must outlive the node — libwaku holds a raw pointer to it.
        let _event_cb_guard = waku.set_event_callback(event_closure);

        let _ = ready_tx.send(Ok(local_enr));

        let topic = pubsub_topic();
        while let Ok(cmd) = out_rx.recv() {
            let res = Self::do_publish(&waku, &topic, cmd.pkt);
            let _ = cmd.reply.try_send(res);
        }

        info!("Waku outbound loop finished");
    }

    fn do_publish(
        waku: &WakuNodeCtx,
        pubsub_topic: &str,
        pkt: OutboundPacket,
    ) -> Result<String, DeliveryServiceError> {
        let content_topic =
            build_content_topic(&pkt.conversation_id, CONVERSATION_VERSION, &pkt.subtopic);
        let payload_b64 = BASE64.encode(&pkt.payload);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let meta_b64 = BASE64.encode(&pkt.app_id);

        let msg_json = serde_json::json!({
            "payload": payload_b64,
            "contentTopic": content_topic,
            "timestamp": timestamp as u64,
            "version": 2,
            "meta": meta_b64,
        })
        .to_string();

        waku.relay_publish(pubsub_topic, &msg_json, 10_000)
            .map_err(|e| {
                error!("Failed to relay publish: {e}");
                DeliveryServiceError::WakuPublish(e)
            })
    }

    /// Parse a waku event JSON into an [`InboundPacket`]. Returns `None`
    /// for non-message events or malformed payloads.
    fn parse_event(data: &str) -> Option<InboundPacket> {
        let v: serde_json::Value = serde_json::from_str(data).ok()?;

        let waku_msg = v.get("wakuMessage")?;
        let payload_b64 = waku_msg.get("payload")?.as_str()?;
        let payload = BASE64.decode(payload_b64).ok()?;
        let content_topic = waku_msg.get("contentTopic")?.as_str()?;
        let timestamp = waku_msg
            .get("timestamp")
            .and_then(|t| t.as_i64())
            .unwrap_or(0);

        let meta = waku_msg
            .get("meta")
            .and_then(|m| m.as_str())
            .and_then(|m| BASE64.decode(m).ok())
            .unwrap_or_default();

        let (conversation_id, subtopic) = Self::parse_content_topic(content_topic)?;

        debug!("Inbound message: conversation={conversation_id} subtopic={subtopic}");

        Some(InboundPacket {
            payload,
            subtopic,
            conversation_id,
            app_id: meta,
            timestamp,
        })
    }

    /// Split `/{conversation_id}/{version}/{subtopic}/proto` into
    /// `(conversation_id, subtopic)`.
    fn parse_content_topic(ct: &str) -> Option<(String, String)> {
        let mut parts = ct.split('/');
        let _empty = parts.next()?;
        let conversation_id = parts.next()?;
        let _version = parts.next()?;
        let subtopic = parts.next()?;
        if conversation_id.is_empty() || subtopic.is_empty() {
            return None;
        }
        Some((conversation_id.to_owned(), subtopic.to_owned()))
    }
}

impl WakuDeliveryService {
    /// Open a pull-style inbound channel. Each call creates a fresh
    /// receiver; the matching sender is pruned next time an inbound
    /// packet finds it disconnected.
    pub fn inbound_receiver(&self) -> mpsc::Receiver<InboundPacket> {
        let (tx, rx) = mpsc::sync_channel(256);
        match self.subscribers.lock() {
            Ok(mut g) => g.push(tx),
            Err(e) => {
                error!("Subscriber mutex poisoned, subscription lost: {e}");
            }
        }
        rx
    }
}

impl DeliveryService for WakuDeliveryService {
    type Error = DeliveryServiceError;

    fn publish(&mut self, packet: OutboundPacket) -> Result<(), Self::Error> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.outbound
            .send(OutboundCommand {
                pkt: packet,
                reply: reply_tx,
            })
            .map_err(|_| DeliveryServiceError::WakuChannelClosed)?;

        reply_rx
            .recv()
            .map_err(|_| DeliveryServiceError::WakuChannelClosed)??;
        Ok(())
    }

    fn subscribe(&mut self, _delivery_address: &str) -> Result<(), Self::Error> {
        // The Waku node accepts every packet on the configured pubsub
        // topic; per-address filtering is done downstream by `TopicFilter`.
        Ok(())
    }
}
