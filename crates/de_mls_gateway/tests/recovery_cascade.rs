//! Recovery-flow integration tests.
//!
//! Concurrent joins, PendingJoin buffer hygiene, and stale-buffered-invite
//! prevention so a future-steward member doesn't propose Adds for
//! already-joined identities.

mod common;

use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use de_mls::core::StewardListConfig;
use de_mls::defaults::{DefaultConsensusPlugin, DefaultConversationPluginsFactory};
use de_mls::session::ConversationConfig;
use de_mls_ds::{
    DeliveryService, DeliveryServiceError, OutboundPacket, SharedDeliveryService, WELCOME_SUBTOPIC,
};
use de_mls_gateway::user::{Inbound, User};

/// Test-only transport: captures every outbound packet for later inspection
/// instead of sending it. `subscribe` is a no-op — tests deliver inbound
/// by routing captured packets into the user's inbound entry directly.
#[derive(Debug, Default)]
struct H {
    packets: Vec<OutboundPacket>,
}

impl H {
    fn handle() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self::default()))
    }

    fn drain_packets(&mut self) -> Vec<OutboundPacket> {
        std::mem::take(&mut self.packets)
    }
}

impl DeliveryService for H {
    type Error = DeliveryServiceError;

    fn publish(&mut self, packet: OutboundPacket) -> Result<(), Self::Error> {
        self.packets.push(packet);
        Ok(())
    }

    fn subscribe(&mut self, _delivery_address: &str) -> Result<(), Self::Error> {
        Ok(())
    }
}

const ALICE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE_KEY: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const DAVE_KEY: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

type TU = User<DefaultConsensusPlugin, DefaultConversationPluginsFactory>;

fn make(key: &str, cfg: ConversationConfig, steward_cfg: StewardListConfig) -> (TU, Arc<Mutex<H>>) {
    let h = H::handle();
    let mut u = common::wallet::user_from_private_key(key, h.clone() as SharedDeliveryService, cfg);
    u.set_default_steward_list_config(steward_cfg);
    (u, h)
}

fn route(user: &TU, p: &OutboundPacket) {
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

fn settle() {
    sleep(Duration::from_millis(100));
}

/// Reproduces the concurrent-join pattern from the real-UI logs:
/// three joiners send KPs before any of them have received a welcome. Each
/// one observes the others' KPs on the broadcast subtopic while still in
/// `PendingJoin`.
///
/// Regression guard for the buffer-hygiene fix: PendingJoin members must
/// NOT buffer those KPs. If they did, they'd carry stale entries into
/// `Working` state, and when rotation elected them as steward later they'd
/// re-propose invites for members already in the group → MLS rejects the
/// batch with "Duplicate signature key".
#[test]
fn concurrent_joins_leave_joiners_with_empty_buffer() {
    let group = "recovery-test";
    let cfg = ConversationConfig {
        commit_inactivity_duration: Duration::from_millis(50),
        freeze_duration: Duration::from_millis(10),
        ..ConversationConfig::default()
    };
    let steward_cfg = StewardListConfig::new(1, 5).unwrap();

    let (mut alice, _ah) = make(ALICE_KEY, cfg.clone(), steward_cfg.clone());
    let (mut bob, bh) = make(BOB_KEY, cfg.clone(), steward_cfg.clone());
    let (mut charlie, ch) = make(CHARLIE_KEY, cfg.clone(), steward_cfg.clone());
    let (mut dave, dh) = make(DAVE_KEY, cfg.clone(), steward_cfg.clone());

    // Step 1: alice creates the group. Bob/Charlie/Dave register as joiners.
    alice.start_conversation(group, true).unwrap();
    bob.start_conversation(group, false).unwrap();
    charlie.start_conversation(group, false).unwrap();
    dave.start_conversation(group, false).unwrap();

    // Step 2: All three joiners announce KPs nearly simultaneously.
    // Key-package send is user-level and publishes straight to the user's
    // transport handle.
    for u in [&bob, &charlie, &dave] {
        let kp = u.generate_key_package().unwrap();
        u.send_key_package(group, kp).unwrap();
    }

    // Step 3: Broadcast every KP packet to every participant (mocks pubsub).
    // Each joiner receives its own KP + the others'. Alice receives all three.
    let mut all_kp_packets = vec![];
    for h in [&bh, &ch, &dh] {
        all_kp_packets.extend(h.lock().unwrap().drain_packets());
    }
    for p in &all_kp_packets {
        route(&alice, p);
        route(&bob, p);
        route(&charlie, p);
        route(&dave, p);
    }
    settle();

    // Regression guard: PendingJoin members must have empty pending_updates
    // buffers. Alice (steward) has the KPs in her buffer until she commits.
    for (name, user) in [("bob", &bob), ("charlie", &charlie), ("dave", &dave)] {
        let session = user.lookup_entry(group).unwrap().unwrap();
        let count = session.read().unwrap().pending_update_count();
        assert_eq!(
            count, 0,
            "{name} in PendingJoin must not buffer broadcast KPs"
        );
    }
}
