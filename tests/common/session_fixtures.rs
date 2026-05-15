//! Integration-test fixtures for SessionRunner-driven scenarios.
//!
//! Built around [`User`] + [`crate::app::SessionRunner`] over the
//! `DefaultConversationPluginsFactory`. Every helper drives the production
//! public surface — no peeking at private state. Packet relay is explicit:
//! tests drain a [`CapturingTransport`] and call `process_inbound_packet`
//! on the receivers, mirroring what `recovery_cascade.rs` established.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use de_mls::app::{ConversationConfig, DefaultConversationPluginsFactory, SessionRunner, User};
use de_mls::core::{DefaultConsensusPlugin, StewardListConfig};
use de_mls::ds::{DeliveryService, DeliveryServiceError, InboundPacket, OutboundPacket};
use prost::Message;
use tokio::sync::RwLock;

pub type TestUser = User<DefaultConsensusPlugin, DefaultConversationPluginsFactory>;
pub type TestSession = SessionRunner<DefaultConsensusPlugin, DefaultConversationPluginsFactory>;
pub type SessionArc = Arc<RwLock<TestSession>>;

/// Test transport that captures every outbound packet for later inspection
/// instead of sending. `subscribe()` returns a dangling receiver — tests
/// deliver inbound explicitly via `process_inbound_packet`.
pub struct CapturingTransport {
    packets: Mutex<Vec<OutboundPacket>>,
}

impl CapturingTransport {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            packets: Mutex::new(Vec::new()),
        })
    }

    pub fn drain_packets(&self) -> Vec<OutboundPacket> {
        std::mem::take(&mut *self.packets.lock().unwrap())
    }

    pub fn snapshot(&self) -> Vec<OutboundPacket> {
        self.packets.lock().unwrap().clone()
    }

    pub fn count_matching(&self, pred: impl Fn(&OutboundPacket) -> bool) -> usize {
        self.packets
            .lock()
            .unwrap()
            .iter()
            .filter(|p| pred(p))
            .count()
    }

    pub fn drain_matching(&self, pred: impl Fn(&OutboundPacket) -> bool) -> Vec<OutboundPacket> {
        let mut guard = self.packets.lock().unwrap();
        let (matching, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut *guard).into_iter().partition(pred);
        *guard = rest;
        matching
    }
}

impl DeliveryService for CapturingTransport {
    fn send(&self, pkt: OutboundPacket) -> Result<String, DeliveryServiceError> {
        self.packets.lock().unwrap().push(pkt);
        Ok("ok".into())
    }
    fn subscribe(&self) -> std::sync::mpsc::Receiver<InboundPacket> {
        let (_tx, rx) = std::sync::mpsc::channel();
        rx
    }
}

/// Predicates for matching packets on the wire. Each one decodes the
/// payload best-effort and matches the relevant variant; an undecodable
/// payload returns `false` rather than panicking.
pub mod predicate {
    use super::*;
    use de_mls::ds::{APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
    use de_mls::protos::de_mls::messages::v1::{
        AppMessage, WelcomeMessage, app_message, welcome_message,
    };

    pub fn is_app_msg(p: &OutboundPacket) -> bool {
        p.subtopic == APP_MSG_SUBTOPIC
    }

    pub fn is_welcome(p: &OutboundPacket) -> bool {
        p.subtopic == WELCOME_SUBTOPIC
    }

    pub fn is_commit_candidate(p: &OutboundPacket) -> bool {
        if p.subtopic != APP_MSG_SUBTOPIC {
            return false;
        }
        let Ok(msg) = AppMessage::decode(p.payload.as_slice()) else {
            return false;
        };
        matches!(msg.payload, Some(app_message::Payload::CommitCandidate(_)))
    }

    pub fn is_conversation_sync(p: &OutboundPacket) -> bool {
        if p.subtopic != APP_MSG_SUBTOPIC {
            return false;
        }
        let Ok(msg) = AppMessage::decode(p.payload.as_slice()) else {
            return false;
        };
        matches!(msg.payload, Some(app_message::Payload::ConversationSync(_)))
    }

    pub fn is_kp(p: &OutboundPacket) -> bool {
        if p.subtopic != WELCOME_SUBTOPIC {
            return false;
        }
        let Ok(msg) = WelcomeMessage::decode(p.payload.as_slice()) else {
            return false;
        };
        matches!(
            msg.payload,
            Some(welcome_message::Payload::UserKeyPackage(_))
        )
    }

    pub fn is_invitation(p: &OutboundPacket) -> bool {
        if p.subtopic != WELCOME_SUBTOPIC {
            return false;
        }
        let Ok(msg) = WelcomeMessage::decode(p.payload.as_slice()) else {
            return false;
        };
        matches!(
            msg.payload,
            Some(welcome_message::Payload::InvitationToJoin(_))
        )
    }
}

/// Build a [`TestUser`] with a [`CapturingTransport`] using the given
/// config and steward-list config.
pub fn make_user(
    private_key: &str,
    cfg: ConversationConfig,
    steward_cfg: StewardListConfig,
) -> (TestUser, Arc<CapturingTransport>) {
    let transport = CapturingTransport::new();
    let mut user = User::with_private_key_and_config(
        private_key,
        transport.clone() as Arc<dyn DeliveryService>,
        cfg,
    )
    .expect("build TestUser");
    user.set_default_steward_list_config(steward_cfg);
    (user, transport)
}

/// Convert an outbound packet (captured from one transport) into an
/// inbound packet ready to feed into another User's `process_inbound_packet`.
pub fn to_inbound(p: &OutboundPacket) -> InboundPacket {
    InboundPacket::new(
        p.payload.clone(),
        &p.subtopic,
        &p.conversation_id,
        p.app_id.clone(),
        0,
    )
}

/// Sleep 100 ms — recovery_cascade.rs convention for letting an async
/// poll loop catch up after a single round of state changes.
pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
}

/// Sleep `d` — explicit timing for inactivity-window tests.
pub async fn settle_for(d: Duration) {
    tokio::time::sleep(d).await;
}

/// Deliver one packet to a single user. Returns the raw `process_inbound_packet`
/// result so the caller can assert success or error.
pub async fn deliver(user: &TestUser, p: &OutboundPacket) {
    let _ = user.process_inbound_packet(to_inbound(p)).await;
}

/// Deliver each packet to every receiver. Errors are swallowed (mirrors
/// pubsub-style relay where a delivery failure on one node doesn't stop
/// the relay).
pub async fn broadcast(packets: &[OutboundPacket], receivers: &[&TestUser]) {
    for p in packets {
        for r in receivers {
            let _ = r.process_inbound_packet(to_inbound(p)).await;
        }
    }
}
