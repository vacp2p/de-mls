//! Waku transport implementation and Waku-backed `DeliveryService`.

pub(crate) mod sys;
pub(crate) mod wrapper;

use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use tracing::{debug, error, info};

use crate::ds::{
    transport::{DeliveryService, InboundPacket, OutboundPacket},
    DeliveryServiceError,
};
use wrapper::WakuNodeCtx;

use super::{GROUP_VERSION, SUBTOPICS};

/// The pubsub topic for the Waku Node.
pub fn pubsub_topic() -> String {
    "/waku/2/rs/15/1".to_string()
}

/// Build the content topics for a group.
pub fn build_content_topics(group_name: &str) -> Vec<String> {
    SUBTOPICS
        .iter()
        .map(|subtopic| build_content_topic(group_name, GROUP_VERSION, subtopic))
        .collect()
}

/// Build the content topic string: `/{group_name}/{version}/{subtopic}/proto`
pub fn build_content_topic(group_name: &str, group_version: &str, subtopic: &str) -> String {
    format!("/{group_name}/{group_version}/{subtopic}/proto")
}

// ── Outbound command ────────────────────────────────────────────────────────
struct OutboundCommand {
    pkt: OutboundPacket,
    reply: mpsc::SyncSender<Result<String, DeliveryServiceError>>,
}

// ── Subscriber registry ─────────────────────────────────────────────────────
type SubscriberList = Arc<Mutex<Vec<mpsc::SyncSender<InboundPacket>>>>;

/// Result returned by [`WakuDeliveryService::start`].
pub struct WakuStartResult {
    pub service: WakuDeliveryService,
    /// The local ENR (Ethereum Node Record) if discv5 is enabled.
    /// Pass this to other nodes via `WakuConfig::discv5_bootstrap_enrs`.
    pub enr: Option<String>,
}

/// Waku-backed delivery service.
///
/// The service starts an embedded Waku node on a dedicated `std::thread`.
/// All interaction is via synchronous `std::sync::mpsc` channels.
///
/// Use [`WakuDeliveryService::start`] to create an instance. Call
/// [`shutdown`](WakuDeliveryService::shutdown) for explicit cleanup, or
/// simply drop all clones to stop the background thread.
#[derive(Clone)]
pub struct WakuDeliveryService {
    outbound: mpsc::SyncSender<OutboundCommand>,
    subscribers: SubscriberList,
    enr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WakuConfig {
    pub node_port: u16,
    /// Enable discv5 peer discovery. Nodes on the same clusterId/shard
    /// will find each other automatically.
    pub discv5: bool,
    /// UDP port for discv5 discovery (default: 9000).
    pub discv5_udp_port: u16,
    /// Bootstrap ENR strings for discv5. If empty and discv5 is enabled,
    /// this node acts as a bootstrap node (others must know its ENR).
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
    /// Start a Waku node and return both the delivery service and the local ENR
    /// (if discv5 is enabled).
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
            .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))?;

        // Wait for the node to either start or fail.
        let enr = ready_rx
            .recv()
            .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))??;

        let service = Self {
            outbound: out_tx,
            subscribers,
            enr: enr.clone(),
        };
        Ok(WakuStartResult { service, enr })
    }

    /// The local ENR (Ethereum Node Record), available when discv5 is enabled.
    pub fn enr(&self) -> Option<&str> {
        self.enr.as_deref()
    }

    /// Explicitly shut down the background Waku node thread.
    ///
    /// After calling this, all subsequent [`send`](DeliveryService::send) calls
    /// will return an error. Alternatively, dropping all clones of this service
    /// achieves the same effect.
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
            "logLevel": "FATAL",
        });

        if cfg.discv5 {
            config["discv5Discovery"] = serde_json::json!(true);
            config["discv5UdpPort"] = serde_json::json!(cfg.discv5_udp_port);
            if !cfg.discv5_bootstrap_enrs.is_empty() {
                config["discv5BootstrapNodes"] = serde_json::json!(cfg.discv5_bootstrap_enrs);
            }
        }

        let config_json = config.to_string();

        // Create node
        let waku = match WakuNodeCtx::new(&config_json) {
            Ok(w) => w,
            Err(e) => {
                let _ = ready_tx.send(Err(DeliveryServiceError::WakuNodeAlreadyInitialized(e)));
                return;
            }
        };

        // Start node
        if let Err(e) = waku.start() {
            let _ = ready_tx.send(Err(DeliveryServiceError::WakuNodeAlreadyInitialized(e)));
            return;
        }
        info!("Waku node started");

        thread::sleep(Duration::from_secs(2));

        // Retrieve ENR for discv5 bootstrapping (discv5 is started automatically
        // by waku_start when discv5Discovery=true is in the config JSON).
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

        // Explicit relay subscribe as safety net (config shards may auto-subscribe,
        // but this ensures we're subscribed regardless of libwaku version behavior).
        let topic = pubsub_topic();
        if let Err(e) = waku.relay_subscribe(&topic) {
            // Non-fatal: some libwaku versions reject duplicate subscribe
            info!("relay_subscribe returned (may already be subscribed): {e}");
        }

        // Set event callback — this closure must live for the node lifetime.
        let subs_for_cb = subscribers.clone();
        let event_closure = move |_ret: i32, data: &str| {
            if let Some(pkt) = Self::parse_event(data) {
                let guard = subs_for_cb.lock();
                // If the mutex is poisoned, subscribers are lost — log and skip.
                let mut guard = match guard {
                    Ok(g) => g,
                    Err(e) => {
                        error!("Subscriber mutex poisoned: {e}");
                        return;
                    }
                };
                guard.retain(|tx| match tx.try_send(pkt.clone()) {
                    Ok(()) => true,
                    Err(mpsc::TrySendError::Full(_)) => true, // keep — just slow
                    Err(mpsc::TrySendError::Disconnected(_)) => false, // drop — dead
                });
            }
        };
        // Heap-allocated closure — must stay alive until node stops.
        let _event_cb_guard = waku.set_event_callback(event_closure);

        // Signal ready (with ENR)
        let _ = ready_tx.send(Ok(local_enr));

        // Outbound loop — blocks until all senders drop
        let topic = pubsub_topic();
        while let Ok(cmd) = out_rx.recv() {
            let res = Self::do_publish(&waku, &topic, cmd.pkt);
            let _ = cmd.reply.try_send(res);
        }

        // _event_cb_guard dropped here, then waku dropped (calls waku_stop via Drop)
        info!("Waku outbound loop finished");
    }

    fn do_publish(
        waku: &WakuNodeCtx,
        pubsub_topic: &str,
        pkt: OutboundPacket,
    ) -> Result<String, DeliveryServiceError> {
        let content_topic = build_content_topic(&pkt.group_id, GROUP_VERSION, &pkt.subtopic);
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
                DeliveryServiceError::WakuPublishMessageError(e)
            })
    }

    /// Parse a waku event JSON into an InboundPacket (if it's a message event).
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

        // Parse content topic: /{group_name}/{version}/{subtopic}/proto
        let (group_id, subtopic) = Self::parse_content_topic(content_topic)?;

        debug!("Inbound message: group={group_id} subtopic={subtopic}");

        Some(InboundPacket {
            payload,
            subtopic,
            group_id,
            app_id: meta,
            timestamp,
        })
    }

    /// Parse `/{group_name}/{version}/{subtopic}/proto` into (group_id, subtopic).
    fn parse_content_topic(ct: &str) -> Option<(String, String)> {
        // Expected: "/{group_name}/{version}/{subtopic}/proto"
        let mut parts = ct.split('/');
        let _empty = parts.next()?; // leading ""
        let group_id = parts.next()?;
        let _version = parts.next()?;
        let subtopic = parts.next()?;
        if group_id.is_empty() || subtopic.is_empty() {
            return None;
        }
        Some((group_id.to_owned(), subtopic.to_owned()))
    }
}

impl DeliveryService for WakuDeliveryService {
    fn send(&self, pkt: OutboundPacket) -> Result<String, DeliveryServiceError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.outbound
            .send(OutboundCommand {
                pkt,
                reply: reply_tx,
            })
            .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))?;

        reply_rx
            .recv()
            .map_err(|e| DeliveryServiceError::Other(anyhow::anyhow!(e)))?
    }

    fn subscribe(&self) -> mpsc::Receiver<InboundPacket> {
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
