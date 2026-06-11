//! ConversationSync joiner-bootstrap is idempotent.
//!
//! After bootstrap, a joiner has the steward list (sync was applied during
//! the join). A second sync delivered to that joiner must short-circuit
//! inside `on_conversation_sync` — no state change, no new outbound.

use std::time::Duration;

use de_mls::core::{ConversationState, StewardListConfig};
use de_mls::session::MemberRole;
use de_mls_ds::OutboundPacket;

mod common;
use common::conversation_fixtures::{
    ConversationArc, deliver, fast_test_config, flush_conversation, make_user, poll_once,
    route_welcomes, settle_for,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[test]
fn second_conversation_sync_is_a_no_op() {
    // Manual bootstrap so we can capture the WelcomeReady.conversation_sync_bytes
    // before they are consumed by route_welcomes. Those bytes are the exact
    // sync payload delivered to the joiner on first join; replaying them is the
    // idempotency scenario under test.
    let cfg = fast_test_config();
    let steward_cfg = StewardListConfig::new(1, 3).unwrap();
    let mut users = vec![
        make_user(ALICE, cfg.clone(), steward_cfg.clone()),
        make_user(BOB, cfg, steward_cfg),
    ];

    users[0].0.start_conversation("c2", true).expect("creator");
    users[1].0.start_conversation("c2", false).expect("joiner");

    let sessions: Vec<ConversationArc> = users
        .iter()
        .map(|(u, _)| u.lookup_entry("c2").unwrap().unwrap())
        .collect();

    // Bob sends KP; relay it to alice.
    let kp = users[1].0.generate_key_package().unwrap();
    users[1].0.send_key_package("c2", kp).unwrap();
    let kp_packets: Vec<_> = users[1].1.lock().unwrap().drain_packets();
    for p in &kp_packets {
        deliver(&users[0].0, p);
    }

    // Drive polling rounds until bob is Working, capturing sync bytes on the
    // round where the welcome is delivered.
    let mut captured_sync: Vec<u8> = Vec::new();
    let alice_app_id = users[0].0.app_id().to_vec();
    for _ in 0..30 {
        settle_for(Duration::from_millis(60));
        for (i, (_, h)) in users.iter().enumerate() {
            flush_conversation(&sessions[i], h);
        }
        poll_once(&sessions[0]);
        poll_once(&sessions[1]);
        for (i, (_, h)) in users.iter().enumerate() {
            flush_conversation(&sessions[i], h);
        }

        let (_, sync_bytes) = route_welcomes(&sessions, &mut users);
        if !sync_bytes.is_empty() {
            captured_sync = sync_bytes.into_iter().next().unwrap();
        }

        let mut packets = Vec::new();
        for (_, h) in &users {
            packets.extend(h.lock().unwrap().drain_packets());
        }
        for p in &packets {
            for (u, _) in &users {
                deliver(u, p);
            }
        }

        if sessions[1].read().unwrap().state() == ConversationState::Working {
            break;
        }
    }

    assert!(
        !captured_sync.is_empty(),
        "bootstrap must produce a ConversationSync for the joiner"
    );
    assert_eq!(
        sessions[1].read().unwrap().state(),
        ConversationState::Working,
        "bob must be Working after bootstrap"
    );

    let bob_session = sessions[1].clone();
    let bob_tx = users[1].1.clone();

    // Bootstrap-driven sync left bob with steward-list state.
    let roles_before = bob_session.read().unwrap().member_roles().unwrap();
    assert!(
        roles_before.iter().any(|(_, r)| matches!(
            r,
            MemberRole::EpochSteward | MemberRole::BackupSteward | MemberRole::Steward
        )),
        "bob must see at least one steward after bootstrap, got {roles_before:?}"
    );
    let scores_before = bob_session.read().unwrap().member_scores();

    // Deliver the captured sync bytes to bob again as a second delivery.
    let sync_packet = OutboundPacket::broadcast("c2", &alice_app_id, captured_sync);
    bob_tx.lock().unwrap().drain_packets();
    deliver(&users[1].0, &sync_packet);
    let bob_outbound_after = bob_tx.lock().unwrap().drain_packets();
    let roles_after = bob_session.read().unwrap().member_roles().unwrap();
    let scores_after = bob_session.read().unwrap().member_scores();

    assert!(
        bob_outbound_after.is_empty(),
        "second sync must not produce any outbound packets, got {bob_outbound_after:?}"
    );
    assert_eq!(
        roles_before, roles_after,
        "second sync must not change member roles"
    );
    assert_eq!(
        scores_before, scores_after,
        "second sync must not change member scores"
    );
}
