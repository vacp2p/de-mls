//! Delivery service — transport-agnostic messaging layer.
//!
//! Defines the [`DeliveryService`] trait +
//! its envelopes ([`OutboundPacket`],
//! [`InboundPacket`]), the
//! [`DeliveryServiceError`] enum, and
//! the [`TopicFilter`] used by the app as a
//! fast allowlist. A reference Waku-backed implementation lives in
//! `waku/` and is gated by the `waku` cargo feature.

mod error;
mod topic_filter;
mod transport;

#[cfg(feature = "waku")]
mod waku;

/// Protocol version embedded in content topics.
pub const CONVERSATION_VERSION: &str = "1";
/// Subtopic identifier for application messages.
pub const APP_MSG_SUBTOPIC: &str = "app_msg";
/// Subtopic identifier for MLS welcome messages.
pub const WELCOME_SUBTOPIC: &str = "welcome";
/// All subtopics that each conversation subscribes to.
pub const SUBTOPICS: [&str; 2] = [APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC];

pub use error::DeliveryServiceError;
pub use topic_filter::TopicFilter;
pub use transport::{DeliveryService, InboundPacket, OutboundPacket, SharedDeliveryService};

#[cfg(feature = "waku")]
pub use waku::{
    WakuConfig, WakuDeliveryService, WakuStartResult, build_content_topic, build_content_topics,
    pubsub_topic,
};
