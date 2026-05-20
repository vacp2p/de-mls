//! Transport-agnostic envelopes + delivery service interface.
//!
//! [`DeliveryService`] shape:
//! - `Debug` supertrait.
//! - Associated `type Error: Display + Debug` lets each transport define
//!   its own error shape. `WakuDeliveryService` pins
//!   `type Error = DeliveryServiceError`; other transports bring their own.
//! - `publish(&mut self, ...)` and `subscribe(&mut self, &str)` take a
//!   mutable receiver so impls can mutate internal state without interior
//!   mutability. The trait object is stored behind `Arc<Mutex<dyn ...>>`
//!   so concurrent callers serialize on the mutex; single-thread
//!   integrators can wrap in `Rc<RefCell<dyn ...>>` instead.
//! - `subscribe(addr)` registers interest in a delivery address and
//!   returns `Result<(), Error>`. The pull-style `Receiver`-returning API
//!   the Waku impl previously offered now lives as a Waku-specific
//!   concrete method.

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

/// Trait implemented by every transport. `Debug` supertrait and
/// `Error: Display + Debug` only — no `Send`/`Sync` on the trait itself or
/// on the associated `Error`, so single-thread hosts can implement
/// without thread-safety bounds. The `'static` bound is preserved so
/// trait objects (`dyn DeliveryService<Error = …>`) are storable behind
/// `Arc`.
pub trait DeliveryService: Debug + 'static {
    /// Per-impl error type. Pinned to [`DeliveryServiceError`] at storage
    /// sites inside de-mls (`Arc<Mutex<dyn DeliveryService<Error =
    /// DeliveryServiceError>>>`) so existing consumers route everything
    /// through one error enum; integrators that need a richer error type
    /// can wrap in an adapter that maps to [`DeliveryServiceError::Other`].
    type Error: Display + Debug + 'static;

    /// Publish a packet to the network.
    fn publish(&mut self, packet: OutboundPacket) -> Result<(), Self::Error>;

    /// Register interest in a delivery address. The impl is responsible for
    /// routing packets matching this address into the application — de-mls's
    /// Waku transport already delivers to a private broadcast channel
    /// reachable via the concrete `WakuDeliveryService::inbound_receiver`
    /// method, so for it this call is currently a structural no-op.
    fn subscribe(&mut self, delivery_address: &str) -> Result<(), Self::Error>;
}

/// Type alias for the trait object shape de-mls stores internally. Pins
/// the associated `Error` so dyn-dispatch is object-safe and existing
/// `UserError::Transport(#[from] DeliveryServiceError)` conversion works
/// without changes. The `Send` is on the trait-object wrapper (not the
/// trait) so a `User` clone-handle can move between tasks even though the
/// trait itself doesn't require `Send`.
pub type SharedDeliveryService =
    Arc<Mutex<dyn DeliveryService<Error = DeliveryServiceError> + Send>>;
