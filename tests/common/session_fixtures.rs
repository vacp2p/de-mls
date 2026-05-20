//! Integration-test fixtures for SessionRunner-driven scenarios.
//!
//! Built around [`User`] + [`crate::app::SessionRunner`] over the
//! `DefaultConversationPluginsFactory`. Every helper drives the production
//! public surface — no peeking at private state. Packet relay is explicit:
//! tests drain a [`CapturingTransport`] and call `process_inbound_packet`
//! on the receivers, mirroring what `recovery_cascade.rs` established.

#![allow(dead_code)]

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use de_mls::app::{ConversationConfig, DefaultConversationPluginsFactory, SessionRunner, User};
use de_mls::core::{DefaultConsensusPlugin, StewardListConfig};
use de_mls::ds::{
    DeliveryService, DeliveryServiceError, InboundPacket, OutboundPacket, SharedDeliveryService,
};
use prost::Message;
use tokio::task::JoinHandle;

/// Shared handle to the test transport. Tests own one of these per `User`
/// and reach into it via `.lock().unwrap()`.
pub type TransportHandle = Arc<Mutex<CapturingTransport>>;

pub type TestUser = User<DefaultConsensusPlugin, DefaultConversationPluginsFactory>;
pub type TestSession = SessionRunner<DefaultConsensusPlugin, DefaultConversationPluginsFactory>;
pub type SessionArc = Arc<RwLock<TestSession>>;

/// Test transport that captures every outbound packet for later inspection
/// instead of sending. `subscribe` is a no-op — tests deliver inbound
/// explicitly via `process_inbound_packet`.
#[derive(Debug, Default)]
pub struct CapturingTransport {
    packets: Vec<OutboundPacket>,
}

impl CapturingTransport {
    pub fn new() -> TransportHandle {
        Arc::new(Mutex::new(Self::default()))
    }

    pub fn drain_packets(&mut self) -> Vec<OutboundPacket> {
        std::mem::take(&mut self.packets)
    }

    pub fn snapshot(&self) -> Vec<OutboundPacket> {
        self.packets.clone()
    }

    pub fn count_matching(&self, pred: impl Fn(&OutboundPacket) -> bool) -> usize {
        self.packets.iter().filter(|p| pred(p)).count()
    }

    pub fn drain_matching(
        &mut self,
        pred: impl Fn(&OutboundPacket) -> bool,
    ) -> Vec<OutboundPacket> {
        let (matching, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.packets)
            .into_iter()
            .partition(pred);
        self.packets = rest;
        matching
    }
}

impl DeliveryService for CapturingTransport {
    type Error = DeliveryServiceError;

    fn publish(&mut self, packet: OutboundPacket) -> Result<(), Self::Error> {
        self.packets.push(packet);
        Ok(())
    }

    fn subscribe(&mut self, _delivery_address: &str) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Predicates for matching packets on the wire.
///
/// **Outer-envelope asymmetry on `APP_MSG_SUBTOPIC`.** Most app messages
/// (chat, votes, proposals, `ConversationSync`) get a fresh MLS encryption
/// pass in `mls.build_message`, so the outer prost payload is opaque to a
/// non-member observer. `CommitCandidate` skips that pass: the inner
/// `commit_message` field is already MLS-encrypted output of
/// `mls.create_commit_candidate`, and the outer prost wrapper carries the
/// staging metadata peers must read *before* they can apply the commit
/// (steward identity for sender cross-validation, the staged
/// `mls_proposals`). Wrapping it again would force peers to be members of
/// the new epoch before they could even tell whose commit it is.
/// `WELCOME_SUBTOPIC` is similarly plaintext on its outer prost layer.
///
/// In practice: [`is_commit_candidate`], [`is_kp`], and [`is_invitation`]
/// can prost-decode the wire payload. For everything else on the app-msg
/// subtopic, identify packets by ordering / sender state (e.g. "the
/// single packet emitted right after `send_conversation_sync`") rather
/// than by payload inspection.
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

    /// Matches `CommitCandidate` on app-msg subtopic. Works because the
    /// outer prost wrapper carries pre-merge staging metadata and isn't
    /// re-encrypted by `build_message` — see the module doc.
    pub fn is_commit_candidate(p: &OutboundPacket) -> bool {
        if p.subtopic != APP_MSG_SUBTOPIC {
            return false;
        }
        let Ok(msg) = AppMessage::decode(p.payload.as_slice()) else {
            return false;
        };
        matches!(msg.payload, Some(app_message::Payload::CommitCandidate(_)))
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
) -> (TestUser, TransportHandle) {
    let transport = CapturingTransport::new();
    let mut user = User::with_private_key_and_config(
        private_key,
        transport.clone() as SharedDeliveryService,
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

/// Mirror of `de_mls_gateway::Gateway::spawn_consensus_forwarder` — one
/// task per session that subscribes to the session's private consensus
/// event bus and drives `apply_consensus_outcome` on each event.
///
/// **Required for any test that exercises consensus.** Per the per-conv-
/// consensus refactor, each `SessionRunner.consensus` owns a fresh private
/// event bus (see `ConsensusContext::build_service`). The library emits
/// `ConsensusReached` to that bus, but no code in `de-mls` itself consumes
/// it — production has the gateway run this forwarder, tests must too.
/// Without it, proposals reach consensus but never populate
/// `approved_proposals`.
///
/// Task exits naturally when the bus is dropped (conversation removed
/// from the registry).
pub fn spawn_consensus_forwarder(session: SessionArc) -> JoinHandle<()> {
    use hashgraph_like_consensus::events::ConsensusEventBus;
    tokio::spawn(async move {
        let mut rx = session.read().unwrap().consensus.event_bus().subscribe();
        while let Ok((_conversation_name, event)) = rx.recv().await {
            let _ = SessionRunner::apply_consensus_outcome(&session, event).await;
        }
    })
}

/// Default fast-timing config for SessionRunner-driven tests. All inactivity
/// and consensus deadlines are sub-second so a polling loop converges in a
/// handful of rounds. Override individual fields where the test needs
/// different timing.
pub fn fast_test_config() -> ConversationConfig {
    use std::time::Duration;
    ConversationConfig {
        commit_inactivity_duration: Duration::from_millis(50),
        freeze_duration: Duration::from_millis(20),
        voting_delay: Duration::from_millis(30),
        election_voting_delay: Duration::from_millis(30),
        consensus_timeout: Duration::from_millis(150),
        proposal_expiration: Duration::from_millis(2000),
        ..ConversationConfig::default()
    }
}

/// One polling cycle on a session: drive freeze status, member-freeze
/// check, and (for joiners) the pending-join tick. Mirrors the production
/// `group_polling_loop` body in `de_mls_gateway::group`.
pub async fn poll_once(session: &SessionArc) {
    let _ = SessionRunner::poll_freeze_status(session).await;
    let _ = SessionRunner::check_member_freeze(session).await;
    let _ = SessionRunner::check_pending_join(session);
}

/// Bring up a conversation with `keys[0]` as the creator and the rest as
/// joiners. Drives the full join cycle: each joiner sends a KP, the
/// creator promotes them to InviteMember proposals, consensus resolves,
/// commits are made and welcomes broadcast. Returns once every joiner is
/// in [`ConversationState::Working`] AND no packets have flowed for
/// `QUIET_THRESHOLD` consecutive polling rounds.
///
/// The quiet-period exit matters: the InviteMember commit's
/// `on_conversation_updated` handler fires
/// `steward_list_housekeeping` → `try_initiate_steward_election` right as
/// joiners reach Working. If bootstrap exits the instant joiners are
/// Working, that election gets orphaned — its `consensus_timeout` fires
/// without enough votes, `handle_election_rejected` bumps the creator's
/// `retry_round` to 1, and every subsequent `check_member_freeze` call
/// flips to the recovery-inactivity window instead of the commit one.
///
/// One consensus forwarder is spawned per session — its `JoinHandle` is
/// detached and the task lives for the duration of the test process.
///
/// Panics if convergence does not happen within `MAX_ROUNDS` rounds.
pub async fn bootstrap_joined_conversation(
    keys: &[&str],
    conversation: &str,
    cfg: ConversationConfig,
    steward_cfg: StewardListConfig,
) -> Vec<(TestUser, TransportHandle)> {
    use de_mls::core::ConversationState;
    use std::time::Duration;
    const MAX_ROUNDS: usize = 30;
    assert!(!keys.is_empty(), "bootstrap needs at least one key");

    let mut users: Vec<(TestUser, TransportHandle)> = keys
        .iter()
        .map(|k| make_user(k, cfg.clone(), steward_cfg.clone()))
        .collect();

    users[0]
        .0
        .start_conversation(conversation, true)
        .await
        .expect("creator start");
    for (u, _) in users.iter_mut().skip(1) {
        u.start_conversation(conversation, false)
            .await
            .expect("joiner start");
    }

    let mut sessions: Vec<SessionArc> = Vec::with_capacity(users.len());
    for (u, _) in &users {
        sessions.push(
            u.lookup_entry(conversation)
                .unwrap()
                .expect("session registered"),
        );
    }

    // One forwarder per session — without this, consensus events fire
    // but `approved_proposals` never populates. JoinHandles are detached;
    // tasks live for the duration of the test process.
    for s in &sessions {
        std::mem::drop(spawn_consensus_forwarder(s.clone()));
    }

    // Joiners send KPs. Drain joiner transports, deliver to creator.
    for i in 1..users.len() {
        let kp = users[i].0.generate_key_package().expect("kp");
        SessionRunner::send_kp_message(&sessions[i], kp)
            .await
            .expect("send kp");
    }
    let mut kp_packets = Vec::new();
    for (_, h) in users.iter().skip(1) {
        kp_packets.extend(h.lock().unwrap().drain_packets());
    }
    for p in &kp_packets {
        let _ = users[0].0.process_inbound_packet(to_inbound(p)).await;
    }

    // Drive every session's polling and shuttle outbound packets until
    // every joiner is Working AND no packets have flowed for several
    // consecutive rounds. The quiet-period check is important: the
    // post-commit `steward_list_housekeeping` fires an election right
    // as joiners reach Working; if we exit immediately the election
    // gets orphaned (its consensus_timeout fires with no votes counted,
    // and the next session-poll observes `retry_round = 1`).
    const QUIET_THRESHOLD: usize = 3;
    let mut quiet_rounds = 0;
    for round in 0..MAX_ROUNDS {
        tokio::time::sleep(Duration::from_millis(60)).await;
        for s in &sessions {
            poll_once(s).await;
        }
        let mut packets = Vec::new();
        for (_, h) in &users {
            packets.extend(h.lock().unwrap().drain_packets());
        }
        // Deliver each packet to every user. `process_inbound_packet`
        // dedups echoes of our own messages via `app_id`.
        for p in &packets {
            for (u, _) in &users {
                let _ = u.process_inbound_packet(to_inbound(p)).await;
            }
        }

        let mut all_working = true;
        for s in sessions.iter().skip(1) {
            if s.read().unwrap().get_conversation_state() != ConversationState::Working {
                all_working = false;
                break;
            }
        }
        if all_working && packets.is_empty() {
            quiet_rounds += 1;
            if quiet_rounds >= QUIET_THRESHOLD {
                tracing::debug!(rounds = round + 1, "bootstrap converged");
                return users;
            }
        } else {
            quiet_rounds = 0;
        }
    }

    panic!("bootstrap_joined_conversation did not converge after {MAX_ROUNDS} rounds");
}
