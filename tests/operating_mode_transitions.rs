//! Deadlock ECP enters Recovery mode and force-freezes the conversation.
//!
//! `apply_consensus_outcome` matches `RecoveryModeOpened` and calls
//! `enter_recovery_mode` + `force_freezing_and_emit`. The forced phase
//! change is the user-visible signal that Recovery has opened.
//!
//! The Recovery → Normal exit path (in `handle_election_accepted`) needs
//! a recovery election to land; reaching that cleanly from outside
//! requires more scaffolding than this test sets up.  Tracked as a
//! follow-up in `docs/ROADMAP.md`.

use std::time::Duration;

use de_mls::app::{CreatorVote, SessionRunner};
use de_mls::core::{ConversationState, SessionEvent, StewardListConfig};
use de_mls::protos::de_mls::messages::v1::ViolationEvidence;
use tokio::sync::broadcast;

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, fast_test_config, poll_once, settle_for, to_inbound,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[tokio::test]
async fn deadlock_ecp_opens_recovery_and_force_freezes() {
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "b5",
        fast_test_config(),
        StewardListConfig::new(2, 2).unwrap(),
    )
    .await;

    let alice_session = users[0].0.lookup_entry("b5").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("b5").unwrap().unwrap();
    let alice_tx = users[0].1.clone();
    let bob_tx = users[1].1.clone();

    let mut alice_events = alice_session.read().unwrap().subscribe();
    let mut bob_events = bob_session.read().unwrap().subscribe();

    // File a Deadlock ECP. With both members as stewards and bob's
    // auto-vote, the emergency proposal reaches consensus YES.
    let current_epoch = alice_session
        .read()
        .unwrap()
        .get_epoch_and_retry()
        .unwrap()
        .0;
    let request = ViolationEvidence::deadlock(current_epoch)
        .with_creator(b"alice-creator".to_vec())
        .into_update_request()
        .unwrap();
    SessionRunner::initiate_proposal(&alice_session, request, CreatorVote::Yes).unwrap();

    let mut alice_saw_freezing = false;
    let mut bob_saw_freezing = false;
    for _ in 0..30 {
        settle_for(Duration::from_millis(40)).await;
        poll_once(&alice_session).await;
        poll_once(&bob_session).await;
        for p in alice_tx.drain_packets() {
            let _ = users[1].0.process_inbound_packet(to_inbound(&p)).await;
        }
        for p in bob_tx.drain_packets() {
            let _ = users[0].0.process_inbound_packet(to_inbound(&p)).await;
        }

        if any_phase_change(&mut alice_events, ConversationState::Freezing) {
            alice_saw_freezing = true;
        }
        if any_phase_change(&mut bob_events, ConversationState::Freezing) {
            bob_saw_freezing = true;
        }
        if alice_saw_freezing && bob_saw_freezing {
            return;
        }
    }

    panic!(
        "after a Deadlock ECP YES both members must emit PhaseChange(Freezing) \
         (alice={alice_saw_freezing}, bob={bob_saw_freezing})"
    );
}

/// Drain pending events, return true if any was a `PhaseChange(want)`.
fn any_phase_change(rx: &mut broadcast::Receiver<SessionEvent>, want: ConversationState) -> bool {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut found = false;
    loop {
        match rx.try_recv() {
            Ok(SessionEvent::PhaseChange(state)) if state == want => found = true,
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
    found
}
