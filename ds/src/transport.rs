//! Transport-agnostic envelopes + delivery service interface.

use std::future::Future;
use tokio::sync::broadcast;

use crate::DeliveryServiceError;

/// A transport-agnostic packet that should be sent to the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundPacket {
    pub payload: Vec<u8>,
    pub subtopic: String,
    pub group_id: String,
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
    /// Optional transport timestamp (for logging/diagnostics).
    pub timestamp: Option<i64>,
}

impl InboundPacket {
    pub fn new(
        payload: Vec<u8>,
        subtopic: &str,
        group_id: &str,
        app_id: Vec<u8>,
        timestamp: Option<i64>,
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
    fn send(
        &self,
        pkt: OutboundPacket,
    ) -> impl Future<Output = Result<String, DeliveryServiceError>> + Send;

    /// Subscribe to inbound packets.
    fn subscribe(&self) -> broadcast::Receiver<InboundPacket>;
}
