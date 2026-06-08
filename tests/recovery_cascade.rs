//! Recovery-flow integration tests.
//!
//! Concurrent joins, PendingJoin buffer hygiene, and stale-buffered-invite
//! prevention so a future-steward member doesn't propose Adds for
//! already-joined identities.

mod common;

use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

use de_mls::app::{ConversationConfig, User};
use de_mls::core::StewardListConfig;
use de_mls::defaults::{DefaultConsensusPlugin, DefaultConversationPluginsFactory};
use de_mls::ds::{
    DeliveryService, DeliveryServiceError, InboundPacket, OutboundPacket, SharedDeliveryService,
};

/// Test-only transport: captures every outbound packet for later inspection
/// instead of sending it. `subscribe` is a no-op — tests deliver inbound
/// by calling `process_inbound_packet` directly.
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

fn to_in(p: &OutboundPacket) -> InboundPacket {
    InboundPacket::new(
        p.payload.clone(),
        &p.subtopic,
        &p.conversation_id,
        p.app_id.clone(),
        0,
    )
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

    // Step 2: All three joiners send KPs nearly simultaneously. Before the
    // buffer-hygiene fix, each joiner would buffer the others' KPs observed
    // on the broadcast welcome subtopic.
    for u in [&bob, &charlie, &dave] {
        let kp = u.generate_key_package().unwrap();
        let session = u.lookup_entry(group).unwrap().unwrap();
        session.read().unwrap().send_key_package(kp).unwrap();
    }

    // The session is pull-only: drain each joiner's buffered outbound (the
    // KP it just produced) into its transport handle so the relay sees it.
    for (u, h) in [(&bob, &bh), (&charlie, &ch), (&dave, &dh)] {
        let session = u.lookup_entry(group).unwrap().unwrap();
        for pkt in session.read().unwrap().drain_outbound() {
            h.lock().unwrap().publish(pkt).unwrap();
        }
    }

    // Step 3: Broadcast every KP packet to every participant (mocks pubsub).
    // Each joiner receives its own KP + the others'. Alice receives all three.
    let mut all_kp_packets = vec![];
    for h in [&bh, &ch, &dh] {
        all_kp_packets.extend(h.lock().unwrap().drain_packets());
    }
    for p in &all_kp_packets {
        let _ = alice.process_inbound_packet(to_in(p));
        let _ = bob.process_inbound_packet(to_in(p));
        let _ = charlie.process_inbound_packet(to_in(p));
        let _ = dave.process_inbound_packet(to_in(p));
    }
    settle();

    // Regression guard: PendingJoin members must have empty pending_updates
    // buffers. Alice (steward) has the KPs in her buffer until she commits.
    for (name, user) in [("bob", &bob), ("charlie", &charlie), ("dave", &dave)] {
        let session = user.lookup_entry(group).unwrap().unwrap();
        let count = session.read().unwrap().get_pending_update_count();
        assert_eq!(
            count, 0,
            "{name} in PendingJoin must not buffer broadcast KPs"
        );
    }
}
