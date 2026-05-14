//! Recovery-flow integration tests.
//!
//! Concurrent joins, PendingJoin buffer hygiene, and stale-buffered-invite
//! prevention so a future-steward member doesn't propose Adds for
//! already-joined identities.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use de_mls::app::{ConversationConfig, User};
use de_mls::core::{DefaultConsensusPlugin, StewardListConfig};
use de_mls::ds::{DeliveryService, DeliveryServiceError, InboundPacket, OutboundPacket};

/// Test-only transport: captures every outbound packet for later inspection
/// instead of sending it. `subscribe()` returns a dangling receiver so the
/// blocking-channel half goes nowhere (tests deliver inbound by calling
/// `process_inbound_packet` directly).
#[derive(Clone)]
struct H {
    packets: Arc<Mutex<Vec<OutboundPacket>>>,
}

impl H {
    fn new() -> Self {
        Self {
            packets: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn drain_packets(&self) -> Vec<OutboundPacket> {
        std::mem::take(&mut *self.packets.lock().unwrap())
    }
}

impl DeliveryService for H {
    fn send(&self, pkt: OutboundPacket) -> Result<String, DeliveryServiceError> {
        self.packets.lock().unwrap().push(pkt);
        Ok("ok".into())
    }
    fn subscribe(&self) -> std::sync::mpsc::Receiver<InboundPacket> {
        // Inbound is delivered explicitly via `process_inbound_packet` in
        // these tests; the receiver is never polled.
        let (_tx, rx) = std::sync::mpsc::channel();
        rx
    }
}

const ALICE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE_KEY: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const DAVE_KEY: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

type TU = User<DefaultConsensusPlugin, de_mls::app::DefaultConversationPluginsFactory>;

fn make(key: &str, cfg: ConversationConfig, steward_cfg: StewardListConfig) -> (TU, H) {
    let h = H::new();
    let mut u = User::with_private_key_and_config(key, Arc::new(h.clone()), cfg).unwrap();
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

async fn settle() {
    tokio::time::sleep(Duration::from_millis(100)).await;
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
#[tokio::test]
async fn concurrent_joins_leave_joiners_with_empty_buffer() {
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
    alice.start_conversation(group, true).await.unwrap();
    bob.start_conversation(group, false).await.unwrap();
    charlie.start_conversation(group, false).await.unwrap();
    dave.start_conversation(group, false).await.unwrap();

    // Step 2: All three joiners send KPs nearly simultaneously. Before the
    // buffer-hygiene fix, each joiner would buffer the others' KPs observed
    // on the broadcast welcome subtopic.
    bob.send_kp_message(group).await.unwrap();
    charlie.send_kp_message(group).await.unwrap();
    dave.send_kp_message(group).await.unwrap();

    // Step 3: Broadcast every KP packet to every participant (mocks pubsub).
    // Each joiner receives its own KP + the others'. Alice receives all three.
    let mut all_kp_packets = vec![];
    for h in [&bh, &ch, &dh] {
        all_kp_packets.extend(h.drain_packets());
    }
    for p in &all_kp_packets {
        let _ = alice.process_inbound_packet(to_in(p)).await;
        let _ = bob.process_inbound_packet(to_in(p)).await;
        let _ = charlie.process_inbound_packet(to_in(p)).await;
        let _ = dave.process_inbound_packet(to_in(p)).await;
    }
    settle().await;

    // Regression guard: PendingJoin members must have empty pending_updates
    // buffers. Alice (steward) has the KPs in her buffer until she commits.
    assert_eq!(
        bob.get_pending_update_count(group).await.unwrap(),
        0,
        "bob in PendingJoin must not buffer broadcast KPs"
    );
    assert_eq!(
        charlie.get_pending_update_count(group).await.unwrap(),
        0,
        "charlie in PendingJoin must not buffer broadcast KPs"
    );
    assert_eq!(
        dave.get_pending_update_count(group).await.unwrap(),
        0,
        "dave in PendingJoin must not buffer broadcast KPs"
    );
}
