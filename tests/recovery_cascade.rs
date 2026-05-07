//! Recovery-flow integration tests.
//!
//! Concurrent joins, PendingJoin buffer hygiene, and stale-buffered-invite
//! prevention so a future-steward member doesn't propose Adds for
//! already-joined identities.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use hashgraph_like_consensus::service::DefaultConsensusService;

use de_mls::app::{GroupConfig, GroupState, StateChangeHandler, User};
use de_mls::core::{CallbackError, DefaultProvider, GroupEventHandler, StewardListConfig};
use de_mls::ds::{InboundPacket, OutboundPacket};
use de_mls::protos::de_mls::messages::v1::AppMessage;

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

#[async_trait]
impl GroupEventHandler for H {
    async fn on_outbound(&self, _: &str, p: OutboundPacket) -> Result<String, CallbackError> {
        self.packets.lock().unwrap().push(p);
        Ok("ok".into())
    }
    async fn on_app_message(&self, _: &str, _m: AppMessage) -> Result<(), CallbackError> {
        Ok(())
    }
    async fn on_leave_group(&self, _: &str) -> Result<(), CallbackError> {
        Ok(())
    }
    async fn on_joined_group(&self, _: &str) -> Result<(), CallbackError> {
        Ok(())
    }
    async fn on_error(&self, _: &str, _: &str, _: &str) {}
}

#[derive(Clone)]
struct SH;
#[async_trait]
impl StateChangeHandler for SH {
    async fn on_state_changed(&self, _: &str, _: GroupState) {}
}

const ALICE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE_KEY: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const DAVE_KEY: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

type TU = User<
    DefaultProvider,
    de_mls::app::DefaultMlsService,
    de_mls::app::DefaultPeerScoring,
    de_mls::app::DefaultStewardList,
    de_mls::identity::WalletIdentity,
    H,
    SH,
>;

fn make(key: &str, cs: Arc<DefaultConsensusService>, cfg: GroupConfig) -> (TU, H) {
    let h = H::new();
    let u =
        User::with_private_key_and_config(key, cs, Arc::new(h.clone()), Arc::new(SH), cfg).unwrap();
    (u, h)
}

fn to_in(p: &OutboundPacket) -> InboundPacket {
    InboundPacket::new(
        p.payload.clone(),
        &p.subtopic,
        &p.group_id,
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
    let cfg = GroupConfig {
        epoch_duration: Duration::from_millis(50),
        freeze_duration: Duration::from_millis(10),
        protocol: StewardListConfig::new(1, 5).unwrap(),
        ..GroupConfig::default()
    };
    let cs = Arc::new(DefaultConsensusService::new_with_max_sessions(100));

    let (mut alice, _ah) = make(ALICE_KEY, cs.clone(), cfg.clone());
    let (mut bob, bh) = make(BOB_KEY, cs.clone(), cfg.clone());
    let (mut charlie, ch) = make(CHARLIE_KEY, cs.clone(), cfg.clone());
    let (mut dave, dh) = make(DAVE_KEY, cs.clone(), cfg.clone());

    // Step 1: alice creates the group. Bob/Charlie/Dave register as joiners.
    alice.create_group(group, true).await.unwrap();
    bob.create_group(group, false).await.unwrap();
    charlie.create_group(group, false).await.unwrap();
    dave.create_group(group, false).await.unwrap();

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
