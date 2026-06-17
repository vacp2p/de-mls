//! Integration-test fixtures for Conversation-driven scenarios.
//!
//! Built around [`User`] + [`de_mls::session::Conversation`] over the
//! `DefaultConversationPluginsFactory`. Every helper drives the production
//! public surface — no peeking at private state. Packet relay is explicit:
//! tests drain a [`CapturingTransport`] and call `process_inbound_packet`
//! on the receivers, mirroring what `recovery_cascade.rs` established.

#![allow(dead_code)]

use std::sync::{Arc, Mutex, RwLock};
use std::thread::sleep;
use std::time::Duration;

use de_mls::core::StewardListConfig;
use de_mls::defaults::DefaultConsensusPlugin;
use de_mls::session::{Conversation, ConversationConfig};
use de_mls_ds::{
    DeliveryService, DeliveryServiceError, OutboundPacket, SharedDeliveryService, WELCOME_SUBTOPIC,
};
use de_mls_gateway::mls::DefaultConversationPluginsFactory;
use de_mls_gateway::user::{Inbound, User};
use openmls_basic_credential::SignatureKeyPair;
use prost::Message;

use crate::common::wallet::user_from_private_key;

/// Shared handle to the test transport. Tests own one of these per `User`
/// and reach into it via `.lock().unwrap()`.
pub type TransportHandle = Arc<Mutex<CapturingTransport>>;

pub type TestUser =
    User<DefaultConsensusPlugin, DefaultConversationPluginsFactory, SignatureKeyPair>;
pub type TestConversation =
    Conversation<DefaultConsensusPlugin, DefaultConversationPluginsFactory, SignatureKeyPair>;
pub type ConversationArc = Arc<RwLock<TestConversation>>;

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

/// Route a captured outbound packet into another User's inbound entry,
/// mimicking the integrator: the welcome channel carries a key-package
/// announcement, every other channel carries conversation traffic.
fn route_inbound(user: &TestUser, p: &OutboundPacket) {
    let inbound = Inbound {
        conversation_id: p.conversation_id.clone(),
        sender: p.app_id.clone(),
        payload: p.payload.clone(),
    };
    let _ = if p.subtopic == WELCOME_SUBTOPIC {
        user.receive_key_package(inbound)
    } else {
        user.handle_inbound(inbound)
    };
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

/// Deliver one packet to a single user, routed by its channel.
pub fn deliver(user: &TestUser, p: &OutboundPacket) {
    route_inbound(user, p);
}

/// Deliver each packet to every receiver. Errors are swallowed (mirrors
/// pubsub-style relay where a delivery failure on one node doesn't stop
/// the relay).
pub fn broadcast(packets: &[OutboundPacket], receivers: &[&TestUser]) {
    for p in packets {
        for r in receivers {
            route_inbound(r, p);
        }
    }
}

/// Drain `ConversationEvent::WelcomeReady` events from each session and
/// route each locally-minted welcome to the matching joiner via
/// [`TestUser::accept_welcome`] (which also applies the bundled
/// `conversation_sync_bytes`). Returns `(delivered_count,
/// sync_bytes_captured)` — the second element holds every non-empty
/// `conversation_sync_bytes` from delivered welcomes, in order. Call
/// this once per polling round, BEFORE relaying packets — same-round app-msg
/// packets (e.g. the post-commit steward election proposal) need the joiner's
/// MLS attached first.
pub fn route_welcomes(
    sessions: &[ConversationArc],
    users: &mut [(TestUser, TransportHandle)],
) -> (usize, Vec<Vec<u8>>) {
    use de_mls::core::ConversationEvent;
    use de_mls::protos::de_mls::messages::v1::MemberWelcome;

    // Route only minted welcomes — peers re-emit the committer's broadcast
    // as `minted_locally: false`, and routing those too would just bounce
    // off the joiner as duplicates (mirrors the gateway's delivery gate).
    let mut welcomes: Vec<MemberWelcome> = Vec::new();
    for s in sessions.iter() {
        for event in s.read().unwrap().drain_events() {
            if let ConversationEvent::WelcomeReady {
                welcome,
                minted_locally: true,
            } = event
            {
                welcomes.push(welcome);
            }
        }
    }
    let mut delivered = 0;
    let mut sync_bytes_out = Vec::new();
    for welcome in welcomes {
        for (u, _) in users.iter_mut() {
            // Try every user — `welcome_mls` returns `Ok(None)` (which
            // `accept_welcome` surfaces as `Err(WelcomeNotForUs)`) for
            // anyone the welcome doesn't address. Only the targeted
            // joiner attaches MLS and gets the bundled sync applied.
            if u.accept_welcome(&welcome).is_ok() {
                delivered += 1;
                if !welcome.conversation_sync_bytes.is_empty() {
                    sync_bytes_out.push(welcome.conversation_sync_bytes.clone());
                }
            }
        }
    }
    (delivered, sync_bytes_out)
}

/// Default fast-timing config for Conversation-driven tests. All inactivity
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

/// One polling cycle on a session: tick deadlines, advance freeze state,
/// check member-freeze inactivity, and check pending-join expiry. Mirrors the
/// production `group_polling_loop` body in `de_mls_gateway::group`.
pub fn poll_once(session: &ConversationArc) {
    let _ = session.write().unwrap().poll();
}

/// Flush a session's pull-buffered outbound into its user's transport handle
/// so the relay (which drains the handle) picks it up. The session is
/// pull-only — direct session calls in tests buffer here instead of sending;
/// this stands in for the integrator's drain-and-publish.
pub fn flush_conversation(session: &ConversationArc, transport: &TransportHandle) {
    let outbound = session.read().unwrap().drain_outbound();
    let mut t = transport.lock().unwrap();
    for out in outbound {
        t.publish(out.into()).expect("capture publish");
    }
}

/// Flush every conversation's pull-buffered outbound on `user` into its
/// transport handle. Uniform stand-in for the integrator's drain — relay
/// loops call this for each user before draining the handles, regardless of
/// how many sessions the user holds.
pub fn flush_user(user: &TestUser, transport: &TransportHandle) {
    let mut t = transport.lock().unwrap();
    for name in user.list_conversations().unwrap_or_default() {
        if let Ok(Some(session)) = user.lookup_entry(&name) {
            for out in session.read().unwrap().drain_outbound() {
                t.publish(out.into()).expect("capture publish");
            }
        }
    }
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
/// inactivity check in `poll` flips to the recovery-inactivity window instead
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

    let mut sessions: Vec<ConversationArc> = Vec::with_capacity(users.len());
    for (u, _) in &users {
        sessions.push(
            u.lookup_entry(conversation)
                .unwrap()
                .expect("session registered"),
        );
    }

    // Joiners announce KPs. Key-package send is user-level and publishes
    // straight to the user's transport. Drain joiner transports, deliver to
    // creator.
    for (u, _) in users.iter().skip(1) {
        let kp = u.generate_key_package().expect("kp");
        u.send_key_package(conversation, kp).expect("send kp");
    }
    let mut kp_packets = Vec::new();
    for (_, h) in users.iter().skip(1) {
        kp_packets.extend(h.lock().unwrap().drain_packets());
    }
    for p in &kp_packets {
        route_inbound(&users[0].0, p);
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
        for (i, (_, h)) in users.iter().enumerate() {
            flush_conversation(&sessions[i], h);
        }

        // Welcomes never traverse the test transport: the steward emits
        // them as `ConversationEvent::WelcomeReady`. Route each welcome to
        // its joiner BEFORE relaying packets — same-round app-msg
        // traffic (the post-commit steward election proposal) needs
        // the joiner's MLS attached first.
        let (welcome_count, _) = route_welcomes(&sessions, &mut users);
        let delivered_welcome = welcome_count > 0;

        let mut packets = Vec::new();
        for (_, h) in &users {
            packets.extend(h.lock().unwrap().drain_packets());
        }
        // Deliver each packet to every user. Inbound dedups echoes of our
        // own messages via `app_id`.
        for p in &packets {
            for (u, _) in &users {
                route_inbound(u, p);
            }
        }

        let mut all_working = true;
        for s in sessions.iter().skip(1) {
            if s.read().unwrap().state() != ConversationState::Working {
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
