//! ConversationSync joiner-bootstrap is idempotent.
//!
//! After bootstrap, a joiner has the steward list (sync was applied during
//! the join). A second sync delivered to that joiner must short-circuit
//! inside `on_conversation_sync` — no state change, no new outbound.

use std::time::Duration;

use de_mls::app::{MemberRole, SessionRunner};
use de_mls::core::StewardListConfig;

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, deliver, fast_test_config, predicate, settle_for,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[tokio::test]
async fn second_conversation_sync_is_a_no_op() {
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "c2",
        fast_test_config(),
        StewardListConfig::new(1, 3).unwrap(),
    )
    .await;

    let alice_session = users[0].0.lookup_entry("c2").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("c2").unwrap().unwrap();
    let alice_tx = users[0].1.clone();
    let bob_tx = users[1].1.clone();

    // Bootstrap-driven sync left bob with steward-list state. (Proves the
    // first sync delivery applied — without a sync, joiners have no list.)
    let roles_before = bob_session.read().unwrap().get_member_roles().unwrap();
    assert!(
        roles_before.iter().any(|(_, r)| matches!(
            r,
            MemberRole::EpochSteward | MemberRole::BackupSteward | MemberRole::Steward
        )),
        "bob must see at least one steward after bootstrap, got {roles_before:?}"
    );
    let scores_before = bob_session.read().unwrap().get_member_scores();

    // Alice emits a fresh ConversationSync. Drain her transport, find the
    // sync packet, deliver it to bob — the second delivery (the first one
    // landed during bootstrap).
    SessionRunner::send_conversation_sync(&alice_session)
        .await
        .unwrap();
    settle_for(Duration::from_millis(30)).await;

    // ConversationSync packets are MLS-encrypted on the wire, so a prost
    // decode of the payload can't peek inside. Right after we explicitly
    // called `send_conversation_sync`, alice's single outbound app-msg
    // packet IS the sync.
    let outbound = alice_tx.lock().unwrap().drain_packets();
    let sync_packet = outbound
        .iter()
        .find(|p| predicate::is_app_msg(p))
        .cloned()
        .expect("alice must broadcast a sync packet");

    bob_tx.lock().unwrap().drain_packets();
    deliver(&users[1].0, &sync_packet).await;
    let bob_outbound_after = bob_tx.lock().unwrap().drain_packets();
    let roles_after = bob_session.read().unwrap().get_member_roles().unwrap();
    let scores_after = bob_session.read().unwrap().get_member_scores();

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
