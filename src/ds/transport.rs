//! Transport-agnostic envelopes + delivery service interface.
//!
//! The [`DeliveryService`] trait shape mirrors libchat's
//! `core/conversations/src/service_traits.rs` so an integrator that
//! implements one can satisfy the other with a single impl block.
//! Concretely:
//! - `Debug` supertrait.
//! - Associated `type Error: Display` lets each transport define its own
//!   error shape. de-mls's `WakuDeliveryService` pins `type Error =
//!   DeliveryServiceError`; integrators (libchat) bring their own.
//! - `publish(&mut self, ...)` and `subscribe(&mut self, &str)` take a
//!   mutable receiver so impls can mutate internal state without
//!   interior mutability. de-mls stores the trait object behind an
//!   `Arc<Mutex<dyn ...>>` so concurrent callers serialize on the
//!   mutex; single-thread integrators can wrap in `Rc<RefCell<dyn ...>>`
//!   instead.
//! - `subscribe(addr)` registers interest in a delivery address and
//!   returns `Result<(), Error>` — push-style. The `subscribe() ->
//!   Receiver` pull-style API the de-mls Waku impl previously offered
//!   has moved off the trait to a Waku-specific concrete method.

use std::fmt::{Debug, Display};

use crate::ds::DeliveryServiceError;

/// A transport-agnostic packet that should be sent to the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundPacket {
    pub payload: Vec<u8>,
    pub subtopic: String,
    pub conversation_id: String,
    /// Application instance identifier. Transported as the `meta` field on the
    /// wire (Waku message JSON). Used for self-message filtering.
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

    /// Address this packet is delivered to. Used by [`DeliveryService::subscribe`]
    /// callers that want to register interest by the same key the publisher
    /// uses. For de-mls's Waku transport this is the conversation id; other
    /// integrators may derive a different routing key.
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
    /// Transport-provided app instance id / metadata (used for self-message filtering).
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

/// Trait implemented by every transport. The shape is intentionally identical
/// to libchat's `DeliveryService` so an impl can satisfy both crates'
/// requirements with a single set of method bodies.
pub trait DeliveryService: Debug + Send + Sync + 'static {
    /// Per-impl error type. Pinned to [`DeliveryServiceError`] at storage
    /// sites inside de-mls (`Arc<Mutex<dyn DeliveryService<Error =
    /// DeliveryServiceError>>>`) so existing consumers route everything
    /// through one error enum; integrators that need a richer error type
    /// can wrap in an adapter that maps to [`DeliveryServiceError::Other`].
    type Error: Display + Send + Sync + 'static;

    /// Publish a packet to the network.
    fn publish(&mut self, packet: OutboundPacket) -> Result<(), Self::Error>;

    /// Register interest in a delivery address. The impl is responsible for
    /// routing packets matching this address into the application — de-mls's
    /// Waku transport already delivers to a private broadcast channel
    /// reachable via the concrete `WakuDeliveryService::inbound_receiver`
    /// method, so for it this call is currently a structural no-op.
    fn subscribe(&mut self, delivery_address: &str) -> Result<(), Self::Error>;
}

/// Type alias for the trait object shape de-mls stores internally. Pins the
/// associated `Error` so dyn-dispatch is object-safe and existing
/// `UserError::Transport(#[from] DeliveryServiceError)` conversion works
/// without changes.
pub type SharedDeliveryService =
    std::sync::Arc<std::sync::Mutex<dyn DeliveryService<Error = DeliveryServiceError>>>;
