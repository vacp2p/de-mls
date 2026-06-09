//! Transport-agnostic envelopes and the [`DeliveryService`] trait.

use std::{
    fmt::{Debug, Display},
    sync::{Arc, Mutex},
};

use crate::ds::{APP_MSG_SUBTOPIC, DeliveryServiceError, WELCOME_SUBTOPIC};

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

    /// Conversation broadcast traffic (chat, votes, sync, commit candidates).
    /// Owns the application-message subtopic so callers carry no subtopic
    /// knowledge.
    pub fn broadcast(conversation_id: &str, sender: &[u8], payload: Vec<u8>) -> Self {
        Self::new(payload, APP_MSG_SUBTOPIC, conversation_id, sender)
    }

    /// A joiner's key-package announcement, routed on the welcome subtopic so
    /// existing members can propose the joiner for an Add.
    pub fn key_package(conversation_id: &str, sender: &[u8], payload: Vec<u8>) -> Self {
        Self::new(payload, WELCOME_SUBTOPIC, conversation_id, sender)
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
