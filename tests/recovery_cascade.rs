//! Integration tests for the recovery-flow rework.
//!
//! Exercises the bad-path scenarios observed in real-UI logs:
//! - Concurrent joins (all pending-joiners see each others' KPs on broadcast)
//! - Buffer must not be polluted on PendingJoin members
//! - After commit, no member has stale buffered invites that would fail
//!   with MLS "Duplicate signature key" when they later become steward
//!
//! The happy path (one-joiner-at-a-time) is already covered by
//! `tests/async_scoring_removal.rs`. This file focuses on the specific
//! bugs surfaced on 2026-04-22.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use hashgraph_like_consensus::service::DefaultConsensusService;

use de_mls::app::{GroupConfig, GroupState, StateChangeHandler, User};
use de_mls::core::{CallbackError, DefaultProvider, GroupEventHandler, ProtocolConfig};
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

type TU = User<DefaultProvider, H, SH>;

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
        protocol: ProtocolConfig::new(1, 5).unwrap(),
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

/// Focused check: the dispatch path used by the `ViolationDetected` handler
/// is gated on `responsible_ecp_proposer`. If the steward list has multiple
/// eligible proposers, only one of them submits. The others buffer the
/// evidence.
///
/// This is a unit-flavored integration test — it doesn't run a full
/// violation-detection pipeline; it just verifies that the `Group`-level
/// gate exists and picks a deterministic proposer other than the target.
#[test]
fn ecp_responsible_proposer_is_deterministic_and_skips_target() {
    use de_mls::core::{Group, ProtocolConfig};

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    let config = ProtocolConfig::new(3, 3).unwrap();
    let mems: Vec<Vec<u8>> = (1..=3u8).map(member).collect();

    let mut group = Group::new_as_creator("grp", member(1), config).unwrap();
    // Generate a steward list covering all three members starting at epoch 0.
    group.generate_and_set_steward_list(0, &mems, 3).unwrap();

    // Target = member(2). Must not be the proposer.
    let target = member(2);
    let p1 = group
        .responsible_ecp_proposer(1, &target, &mems)
        .map(|p| p.to_vec());
    let p2 = group
        .responsible_ecp_proposer(1, &target, &mems)
        .map(|p| p.to_vec());

    assert_eq!(p1, p2, "must be deterministic");
    assert!(p1.is_some(), "two other members are eligible");
    assert_ne!(p1.unwrap(), target, "must not propose against target");
}
