//! Transport-agnostic message envelopes and delivery service trait.
//!
//! This module defines the core abstraction for sending and receiving
//! messages over any pub/sub transport layer.

use std::future::Future;
use tokio::sync::broadcast;

use crate::ds::DeliveryServiceError;

/// A message to send to the network.
///
/// Contains the encrypted payload and routing metadata needed by the
/// transport layer to deliver the message to the correct recipients.
///
/// # Fields
/// - `payload` - MLS-encrypted message bytes
/// - `subtopic` - Message type (`"welcome"` or `"app"`)
/// - `group_id` - Target group name
/// - `app_id` - Sender's app instance UUID (for self-message filtering)
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

/// A message received from the network.
///
/// Contains the encrypted payload and metadata extracted from the
/// transport layer. The `app_id` is used to filter out messages
/// sent by this same application instance.
///
/// # Fields
/// - `payload` - MLS-encrypted message bytes
/// - `subtopic` - Message type (`"welcome"` or `"app"`)
/// - `group_id` - Source group name
/// - `app_id` - Sender's app instance UUID
/// - `timestamp` - Unix timestamp (milliseconds) from transport
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundPacket {
    pub payload: Vec<u8>,
    pub subtopic: String,
    pub group_id: String,
    /// Sender's app instance ID (used for self-message filtering).
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

/// Trait for transport-agnostic message delivery.
///
/// Implement this trait to integrate DE-MLS with your transport layer.
/// The default implementation [`WakuDeliveryService`](crate::ds::WakuDeliveryService)
/// uses Waku relay for decentralized message delivery.
///
/// # Implementation Requirements
///
/// - Messages must be delivered to all subscribers of the group's content topic
/// - Self-messages (same `app_id`) should be filtered by the caller, not the transport
/// - The transport should handle reconnection and peer discovery
///
/// # Example
///
/// ```ignore
/// struct MyTransport { /* ... */ }
///
/// impl DeliveryService for MyTransport {
///     fn send(&self, pkt: OutboundPacket) -> impl Future<Output = Result<String, DeliveryServiceError>> + Send {
///         async move {
///             let topic = format!("{}/{}", pkt.group_id, pkt.subtopic);
///             self.publish(&topic, &pkt.payload).await?;
///             Ok(uuid::Uuid::new_v4().to_string())
///         }
///     }
///
///     fn subscribe(&self) -> broadcast::Receiver<InboundPacket> {
///         self.incoming_rx.subscribe()
///     }
/// }
/// ```
pub trait DeliveryService: Send + Sync + 'static {
    /// Send a message to the network.
    ///
    /// Returns a transport-provided message ID (for logging/debugging).
    fn send(
        &self,
        pkt: OutboundPacket,
    ) -> impl Future<Output = Result<String, DeliveryServiceError>> + Send;

    /// Subscribe to incoming messages.
    ///
    /// Returns a broadcast receiver that yields all incoming packets.
    /// The caller is responsible for filtering by group membership.
    fn subscribe(&self) -> broadcast::Receiver<InboundPacket>;
}
