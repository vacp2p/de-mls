//! Integration-test fixtures for SessionRunner-driven scenarios.
//!
//! Built around [`User`] + [`crate::app::SessionRunner`] over the
//! `DefaultConversationPluginsFactory`. Every helper drives the production
//! public surface — no peeking at private state. Packet relay is explicit:
//! tests drain a [`CapturingTransport`] and call `process_inbound_packet`
//! on the receivers, mirroring what `recovery_cascade.rs` established.

#![allow(dead_code)]

use std::sync::{Arc, Mutex, RwLock};
use std::thread::sleep;
use std::time::Duration;

use de_mls::app::{ConversationConfig, SessionRunner, User};
use de_mls::core::StewardListConfig;
use de_mls::defaults::{DefaultConsensusPlugin, DefaultConversationPluginsFactory};
use de_mls::ds::{
    DeliveryService, DeliveryServiceError, InboundPacket, OutboundPacket, SharedDeliveryService,
};
use prost::Message;

use crate::common::wallet::user_from_private_key;

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
/// In practice: [`is_commit_candidate`] and [`is_kp`] can prost-decode
/// the wire payload. For everything else on the app-msg subtopic,
/// identify packets by ordering / sender state rather than by payload
/// inspection.
pub mod predicate {
    use super::*;
    use de_mls::ds::{APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
    use de_mls::protos::de_mls::messages::v1::{AppMessage, MemberInvite, app_message};

    pub fn is_app_msg(p: &OutboundPacket) -> bool {
        p.subtopic == APP_MSG_SUBTOPIC
    }

    pub fn is_welcome_subtopic(p: &OutboundPacket) -> bool {
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

    /// Matches a KP-broadcast packet on the welcome subtopic.
    pub fn is_kp(p: &OutboundPacket) -> bool {
        if p.subtopic != WELCOME_SUBTOPIC {
            return false;
        }
        MemberInvite::decode(p.payload.as_slice()).is_ok()
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
    let mut user =
        user_from_private_key(private_key, transport.clone() as SharedDeliveryService, cfg);
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

/// Sleep 100 ms — recovery_cascade.rs convention for letting an
/// poll loop catch up after a single round of state changes.
pub fn settle() {
    sleep(Duration::from_millis(100));
}

/// Sleep `d` — explicit timing for inactivity-window tests.
pub fn settle_for(d: Duration) {
    sleep(d);
}

/// Deliver one packet to a single user. Returns the raw `process_inbound_packet`
/// result so the caller can assert success or error.
pub fn deliver(user: &TestUser, p: &OutboundPacket) {
    let _ = user.process_inbound_packet(to_inbound(p));
}

/// Deliver each packet to every receiver. Errors are swallowed (mirrors
/// pubsub-style relay where a delivery failure on one node doesn't stop
/// the relay).
pub fn broadcast(packets: &[OutboundPacket], receivers: &[&TestUser]) {
    for p in packets {
        for r in receivers {
            let _ = r.process_inbound_packet(to_inbound(p));
        }
    }
}

/// Drain `SessionEvent::WelcomeReady` events from each session and
/// route each welcome to the matching joiner via
/// [`TestUser::accept_welcome`], then replay the bundled
/// `conversation_sync_bytes` through `process_inbound_packet`. Returns
/// the number of welcomes that were successfully accepted. Call this
/// once per polling round, BEFORE relaying packets — same-round
/// app-msg packets (e.g. the post-commit steward election proposal)
/// need the joiner's MLS attached first.
pub fn route_welcomes(sessions: &[SessionArc], users: &mut [(TestUser, TransportHandle)]) -> usize {
    use de_mls::core::SessionEvent;
    use de_mls::protos::de_mls::messages::v1::MemberWelcome;

    // Pair each welcome with its emitter's app_id. The bundled sync is the
    // welcomer's outbound packet, so the replayed `InboundPacket` must carry
    // the welcomer's app_id — replaying it under the joiner's own app_id
    // would trip `process_inbound_packet`'s echo-dedup and silently drop it.
    let mut welcomes: Vec<(MemberWelcome, Vec<u8>)> = Vec::new();
    for (i, s) in sessions.iter().enumerate() {
        let welcomer_app_id = users[i].0.app_id().to_vec();
        for event in s.read().unwrap().drain_events() {
            if let SessionEvent::WelcomeReady(welcome) = event {
                welcomes.push((welcome, welcomer_app_id.clone()));
            }
        }
    }
    let mut delivered = 0;
    for (welcome, welcomer_app_id) in welcomes {
        let conv_name = sessions
            .first()
            .map(|s| s.read().unwrap().conversation_id().to_string())
            .unwrap_or_default();
        for (u, _) in users.iter_mut() {
            // Try every user — `welcome_mls` returns `Ok(None)` (which
            // `accept_welcome` surfaces as `Err(WelcomeNotForUs)`) for
            // anyone the welcome doesn't address. Only the targeted
            // joiner attaches MLS and gets the bundled sync replayed.
            if u.accept_welcome(&welcome.welcome_bytes).is_ok() {
                delivered += 1;
                if !welcome.conversation_sync_bytes.is_empty() {
                    let sync_pkt = de_mls::ds::InboundPacket::new(
                        welcome.conversation_sync_bytes.clone(),
                        de_mls::ds::APP_MSG_SUBTOPIC,
                        &conv_name,
                        welcomer_app_id.clone(),
                        0,
                    );
                    let _ = u.process_inbound_packet(sync_pkt);
                }
            }
        }
    }
    delivered
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
pub fn poll_once(session: &SessionArc) {
    let _ = SessionRunner::tick_deadlines(session);
    let _ = SessionRunner::poll_freeze_status(session);
    let _ = SessionRunner::check_member_freeze(session);
    let _ = SessionRunner::check_pending_join(session);
}

/// Bring up a conversation with `keys[0]` as the creator and the rest as
/// joiners. Drives the full join cycle: each joiner sends a KP, the
/// creator promotes them to InviteMember proposals, consensus resolves,
/// commits are made and welcomes broadcast. Returns once every joiner is
/// in [`ConversationState::Working`] AND no packets have flowed for
/// `QUIET_THRESHOLD` consecutive polling rounds.
///
/// The quiet-period exit matters when the group is large enough to need a
/// voted steward election (`members > sn_max`): the InviteMember commit's
/// `on_conversation_updated` handler fires `steward_list_housekeeping` →
/// `initiate_steward_election` right as joiners reach Working. If bootstrap
/// exits the instant joiners are Working, that election gets orphaned — its
/// `consensus_timeout` fires without enough votes, `handle_election_rejected`
/// bumps the creator's `retry_round` to 1, and every subsequent
/// `check_member_freeze` call flips to the recovery-inactivity window instead
/// of the commit one. Small groups (`members <= sn_max`) reconcile the list
/// locally with no election, so they have nothing to orphan.
///
/// Panics if convergence does not happen within `MAX_ROUNDS` rounds.
pub fn bootstrap_joined_conversation(
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
        .expect("creator start");
    for (u, _) in users.iter_mut().skip(1) {
        u.start_conversation(conversation, false)
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

    // Joiners send KPs. Drain joiner transports, deliver to creator.
    for i in 1..users.len() {
        let kp = users[i].0.generate_key_package().expect("kp");
        SessionRunner::send_key_package(&sessions[i], kp).expect("send kp");
    }
    let mut kp_packets = Vec::new();
    for (_, h) in users.iter().skip(1) {
        kp_packets.extend(h.lock().unwrap().drain_packets());
    }
    for p in &kp_packets {
        let _ = users[0].0.process_inbound_packet(to_inbound(p));
    }

    // Drive every session's polling and shuttle outbound packets until
    // every joiner is Working AND no packets have flowed for several
    // consecutive rounds. The quiet-period check matters for large groups:
    // post-commit `steward_list_housekeeping` fires a voted election right
    // as joiners reach Working; if we exit immediately the election gets
    // orphaned (its consensus_timeout fires with no votes counted, and the
    // next session-poll observes `retry_round = 1`). Small groups reconcile
    // the list locally with no election.
    const QUIET_THRESHOLD: usize = 3;
    let mut quiet_rounds = 0;
    for round in 0..MAX_ROUNDS {
        sleep(Duration::from_millis(60));
        for s in &sessions {
            poll_once(s);
        }

        // Welcomes never traverse the test transport: the steward emits
        // them as `SessionEvent::WelcomeReady`. Route each welcome to
        // its joiner BEFORE relaying packets — same-round app-msg
        // traffic (the post-commit steward election proposal) needs
        // the joiner's MLS attached first.
        let delivered_welcome = route_welcomes(&sessions, &mut users) > 0;

        let mut packets = Vec::new();
        for (_, h) in &users {
            packets.extend(h.lock().unwrap().drain_packets());
        }
        // Deliver each packet to every user. `process_inbound_packet`
        // dedups echoes of our own messages via `app_id`.
        for p in &packets {
            for (u, _) in &users {
                let _ = u.process_inbound_packet(to_inbound(p));
            }
        }

        let mut all_working = true;
        for s in sessions.iter().skip(1) {
            if s.read().unwrap().get_conversation_state() != ConversationState::Working {
                all_working = false;
                break;
            }
        }
        if all_working && packets.is_empty() && !delivered_welcome {
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
