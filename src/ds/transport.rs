//! Transport-agnostic envelopes and the [`DeliveryService`] trait.

use std::{
    fmt::{Debug, Display},
    sync::{Arc, Mutex},
};

use crate::ds::DeliveryServiceError;

/// A transport-agnostic packet that should be sent to the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundPacket {
    pub payload: Vec<u8>,
    pub subtopic: String,
    pub conversation_id: String,
    /// Application instance identifier. Used for self-message filtering.
    pub app_id: Vec<u8>,
}

impl OutboundPacket {
    pub fn new(payload: Vec<u8>, subtopic: &str, conversation_id: &str, app_id: &[u8]) -> Self {
        Self {
            payload,
            subtopic: subtopic.to_string(),
            conversation_id: conversation_id.to_string(),
            app_id: app_id.to_vec(),
        }
    }

    /// Address this packet is delivered to.
    pub fn delivery_address(&self) -> &str {
        &self.conversation_id
    }
}

/// A transport-agnostic packet delivered from the network into the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundPacket {
    pub payload: Vec<u8>,
    pub subtopic: String,
    pub conversation_id: String,
    /// Sender's application instance id.
    pub app_id: Vec<u8>,
    pub timestamp: i64,
}

impl InboundPacket {
    pub fn new(
        payload: Vec<u8>,
        subtopic: &str,
        conversation_id: &str,
        app_id: Vec<u8>,
        timestamp: i64,
    ) -> Self {
        Self {
            payload,
            subtopic: subtopic.to_string(),
            conversation_id: conversation_id.to_string(),
            app_id,
            timestamp,
        }
    }
}

pub trait DeliveryService: Debug + 'static {
    type Error: Display + Debug + 'static;

    /// Publish a packet to the network.
    fn publish(&mut self, packet: OutboundPacket) -> Result<(), Self::Error>;

    /// Register interest in a delivery address.
    fn subscribe(&mut self, delivery_address: &str) -> Result<(), Self::Error>;
}

/// Trait-object shape used internally — pinned `Error` so dyn-dispatch
/// is object-safe and `UserError::Transport(#[from] DeliveryServiceError)`
/// resolves.
pub type SharedDeliveryService =
    Arc<Mutex<dyn DeliveryService<Error = DeliveryServiceError> + Send>>;
