//! Steward inactivity → automatic commit candidate.
//!
//! When the steward has approved work, the commit-inactivity timer fires
//! and a `CommitCandidate` packet appears on the wire — no manual
//! `create_commit_candidate` call from the integrator. Validates the
//! unified polling loop's signature behaviour for stewards.

use std::time::Duration;

use de_mls::app::{CreatorVote, SessionRunner};
use de_mls::core::StewardListConfig;
use de_mls::ds::OutboundPacket;
use de_mls::identity::parse_wallet_to_bytes;
use de_mls::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, RemoveMember, conversation_update_request,
};

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, fast_test_config, poll_once, predicate, settle_for, to_inbound,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[tokio::test]
async fn steward_inactivity_fires_commit_candidate() {
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "b1",
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    )
    .await;

    let alice_session = users[0].0.lookup_entry("b1").await.unwrap();
    let bob_session = users[1].0.lookup_entry("b1").await.unwrap();
    let alice_tx = users[0].1.clone();
    let bob_tx = users[1].1.clone();

    assert!(
        alice_session.read().await.is_steward_for_self(),
        "creator must be steward after bootstrap"
    );

    // Alice files a removal of Bob via the public surface. With bob's
    // auto-vote running on his consensus forwarder, the proposal resolves
    // YES and lands in alice's approved queue.
    let bob_id = parse_wallet_to_bytes(&users[1].0.identity_string()).unwrap();
    let request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember { identity: bob_id },
        )),
    };
    SessionRunner::initiate_proposal(&alice_session, request, CreatorVote::Yes)
        .await
        .unwrap();

    // Drive polling + packet relay. Accumulate alice's outbound so we can
    // verify a `CommitCandidate` packet appears — without any explicit
    // `create_commit_candidate` call from the test.
    let mut alice_outbound: Vec<OutboundPacket> = Vec::new();
    for _ in 0..30 {
        settle_for(Duration::from_millis(40)).await;
        poll_once(&alice_session).await;
        poll_once(&bob_session).await;

        let new_alice = alice_tx.drain_packets();
        let new_bob = bob_tx.drain_packets();
        for p in &new_alice {
            let _ = users[1].0.process_inbound_packet(to_inbound(p)).await;
        }
        for p in &new_bob {
            let _ = users[0].0.process_inbound_packet(to_inbound(p)).await;
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
