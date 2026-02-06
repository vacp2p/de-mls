//! Delivery service abstraction for transport-agnostic messaging.
//!
//! This module provides a transport-agnostic interface for sending and receiving
//! MLS-encrypted messages. The default implementation uses Waku, but the traits
//! can be implemented for any pub/sub transport (libp2p, WebSocket, etc.).
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                     DeliveryService                         │
//! ├─────────────────────────────────────────────────────────────┤
//! │  send()       │  Encrypt + broadcast OutboundPacket         │
//! │  subscribe()  │  Receive InboundPacket stream               │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    WakuDeliveryService                      │
//! │         (default implementation using Waku relay)           │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Types
//!
//! - `OutboundPacket` - Message to send (payload + routing metadata)
//! - `InboundPacket` - Message received (payload + sender metadata)
//! - `DeliveryService` - Trait for transport implementations
//! - `TopicFilter` - Fast allowlist for filtering incoming messages
//!
//! # Topic Structure
//!
//! Each group uses two content topics (subtopics):
//! - `welcome` - Key packages and welcome messages (joining flow)
//! - `app` - Application messages (chat, proposals, votes, commits)
//!
//! The `TopicFilter` allows efficient filtering of messages by group membership.
//!
//! # Example
//!
//! ```ignore
//! use de_mls::ds::{WakuDeliveryService, WakuConfig, DeliveryService};
//!
//! // Create Waku delivery service
//! let config = WakuConfig::new(60001, vec![peer_multiaddr]);
//! let waku = WakuDeliveryService::new(config).await?;
//!
//! // Subscribe to incoming messages
//! let mut rx = waku.subscribe();
//! tokio::spawn(async move {
//!     while let Ok(packet) = rx.recv().await {
//!         println!("Received message for group: {}", packet.group_id);
//!     }
//! });
//!
//! // Send a message
//! let packet = OutboundPacket::new(payload, "app", "my-group", &app_id);
//! waku.send(packet).await?;
//! ```

mod error;
mod topic_filter;
mod transport;
mod waku;

pub use error::DeliveryServiceError;
pub use topic_filter::TopicFilter;
pub use transport::{DeliveryService, InboundPacket, OutboundPacket};
pub use waku::{
    build_content_topic, build_content_topics, pubsub_topic, WakuConfig, WakuDeliveryService,
    APP_MSG_SUBTOPIC, GROUP_VERSION, SUBTOPICS, WELCOME_SUBTOPIC,
};
