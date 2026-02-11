//! Transport-agnostic envelopes + delivery service interface.
use crate::ds::DeliveryServiceError;
/// A transport-agnostic packet that should be sent to the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundPacket {
    pub payload: Vec<u8>,
    pub subtopic: String,
    pub group_id: String,
    /// Application instance identifier. Transported as the `meta` field on the
    /// wire (Waku message JSON). Used for self-message filtering.
    pub app_id: Vec<u8>,
}

impl OutboundPacket {
    pub fn new(payload: Vec<u8>, subtopic: &str, group_id: &str, app_id: &[u8]) -> Self {
        Self {
            payload,
            subtopic: subtopic.to_string(),
            group_id: group_id.to_string(),
            app_id: app_id.to_vec(),
        }
    }
}

/// A transport-agnostic packet delivered from the network into the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundPacket {
    pub payload: Vec<u8>,
    pub subtopic: String,
    pub group_id: String,
    /// Transport-provided app instance id / metadata (used for self-message filtering).
    pub app_id: Vec<u8>,
    pub timestamp: i64,
}

impl InboundPacket {
    pub fn new(
        payload: Vec<u8>,
        subtopic: &str,
        group_id: &str,
        app_id: Vec<u8>,
        timestamp: i64,
    ) -> Self {
        Self {
            payload,
            subtopic: subtopic.to_string(),
            group_id: group_id.to_string(),
            app_id,
            timestamp,
        }
    }
}

pub trait DeliveryService: Send + Sync + 'static {
    /// Send a packet to the network and return a transport message id (if available).
    fn send(&self, pkt: OutboundPacket) -> Result<String, DeliveryServiceError>;

    /// Subscribe to inbound packets.
    ///
    /// Each call creates a new channel and registers its sender internally.
    /// Senders are pruned when the corresponding receiver is dropped, but only
    /// during the next inbound event dispatch. Avoid calling this in a loop
    /// without dropping previous receivers.
    fn subscribe(&self) -> std::sync::mpsc::Receiver<InboundPacket>;
}
