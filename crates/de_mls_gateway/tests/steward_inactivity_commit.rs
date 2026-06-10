//! Steward inactivity → automatic commit candidate.
//!
//! When the steward has approved work, the commit-inactivity timer fires
//! and a `CommitCandidate` packet appears on the wire — no manual
//! `create_commit_candidate` call from the integrator. Validates the
//! unified polling loop's signature behaviour for stewards.

use std::time::Duration;

use de_mls::core::StewardListConfig;
use de_mls::member_id::MemberId;
use de_mls::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, RemoveMember, conversation_update_request,
};
use de_mls::session::CreatorVote;
use de_mls_ds::OutboundPacket;

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, deliver, fast_test_config, flush_session, poll_once, predicate,
    settle_for,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[test]
fn steward_inactivity_fires_commit_candidate() {
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "b1",
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    let alice_session = users[0].0.lookup_entry("b1").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("b1").unwrap().unwrap();
    let alice_tx = users[0].1.clone();
    let bob_tx = users[1].1.clone();

    assert!(
        alice_session.read().unwrap().is_steward(),
        "creator must be steward after bootstrap"
    );

    // Alice files a removal of Bob via the public surface. With bob's
    // auto-vote running on his consensus forwarder, the proposal resolves
    // YES and lands in alice's approved queue.
    let bob_id = common::WalletMemberId::from_hex(&users[1].0.member_id_string())
        .member_id_bytes()
        .to_vec();
    let request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember { member_id: bob_id },
        )),
    };
    alice_session
        .write()
        .unwrap()
        .initiate_proposal(request, CreatorVote::Yes)
        .unwrap();

    // Drive polling + packet relay. Accumulate alice's outbound so we can
    // verify a `CommitCandidate` packet appears — without any explicit
    // `create_commit_candidate` call from the test.
    let mut alice_outbound: Vec<OutboundPacket> = Vec::new();
    for _ in 0..30 {
        settle_for(Duration::from_millis(40));
        poll_once(&alice_session);
        poll_once(&bob_session);
        flush_session(&alice_session, &alice_tx);
        flush_session(&bob_session, &bob_tx);

        let new_alice = alice_tx.lock().unwrap().drain_packets();
        let new_bob = bob_tx.lock().unwrap().drain_packets();
        for p in &new_alice {
            deliver(&users[1].0, p);
        }
        for p in &new_bob {
            deliver(&users[0].0, p);
        }
        alice_outbound.extend(new_alice);

        if alice_outbound.iter().any(predicate::is_commit_candidate) {
            return;
        }
    }

    panic!(
        "expected a CommitCandidate packet from alice's inactivity tick; \
         got {} outbound packets, none matching",
        alice_outbound.len()
    );
}
