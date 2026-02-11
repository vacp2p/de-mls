//! Delivery Service — transport-agnostic messaging layer.
//!
//! This module defines the [`DeliveryService`] trait and its supporting types
//! ([`OutboundPacket`], [`InboundPacket`], [`DeliveryServiceError`]), plus a
//! concrete implementation backed by the Waku relay protocol.
//!
//! # Architecture
//!
//! ```text
//! src/ds/
//! ├── transport.rs      DeliveryService trait, OutboundPacket, InboundPacket
//! ├── error.rs          DeliveryServiceError
//! ├── topic_filter.rs   TopicFilter (HashSet-based async allowlist)
//! └── waku/             Waku relay implementation
//!     ├── mod.rs        WakuDeliveryService, WakuConfig, content-topic helpers
//!     ├── sys.rs        Raw FFI bindings to libwaku (C trampoline pattern)
//!     └── wrapper.rs    Safe synchronous WakuNodeCtx wrapper
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use de_mls::ds::{WakuDeliveryService, WakuConfig, DeliveryService, OutboundPacket};
//!
//! // Start the node — blocks until the embedded Waku node is ready.
//! let result = WakuDeliveryService::start(WakuConfig {
//!     node_port: 60000,
//!     discv5: true,
//!     discv5_udp_port: 61000,
//!     ..Default::default()
//! })?;
//!
//! // The local ENR can be passed to other nodes for bootstrapping.
//! if let Some(enr) = &result.enr {
//!     println!("Share this ENR with peers: {enr}");
//! }
//!
//! let ds = result.service;
//!
//! // Subscribe to inbound messages (multiple subscribers allowed).
//! let rx = ds.subscribe();
//! std::thread::spawn(move || {
//!     while let Ok(pkt) = rx.recv() {
//!         println!("got {} bytes for group {}", pkt.payload.len(), pkt.group_id);
//!     }
//! });
//!
//! // Send a message.
//! ds.send(OutboundPacket::new(
//!     b"hello".to_vec(),
//!     "app_msg",
//!     "my-group",
//!     b"app-instance-id",
//! ))?;
//!
//! // Explicit shutdown (or just drop all clones).
//! ds.shutdown();
//! # Ok::<(), de_mls::ds::DeliveryServiceError>(())
//! ```
//!
//! # Threading model
//!
//! The entire DS layer is **synchronous** — no tokio dependency. The Waku
//! implementation runs an embedded node on a dedicated `std::thread`. Callers
//! in an async context should wrap [`DeliveryService::send`] in
//! `tokio::task::spawn_blocking`.

mod error;
mod topic_filter;
mod transport;
mod waku;

pub use error::DeliveryServiceError;
pub use topic_filter::TopicFilter;
pub use transport::{DeliveryService, InboundPacket, OutboundPacket};
pub use waku::{
    build_content_topic, build_content_topics, pubsub_topic, WakuConfig, WakuDeliveryService,
    WakuStartResult, APP_MSG_SUBTOPIC, GROUP_VERSION, SUBTOPICS, WELCOME_SUBTOPIC,
};
