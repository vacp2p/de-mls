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

use de_mls::core::{ConversationState, SessionEvent, StewardListConfig};
use de_mls::protos::de_mls::messages::v1::ViolationEvidence;
use de_mls::session::CreatorVote;

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, deliver, fast_test_config, flush_session, poll_once, settle_for,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[test]
fn deadlock_ecp_opens_recovery_and_force_freezes() {
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "b5",
        fast_test_config(),
        StewardListConfig::new(2, 2).unwrap(),
    );

    let alice_session = users[0].0.lookup_entry("b5").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("b5").unwrap().unwrap();
    let alice_tx = users[0].1.clone();
    let bob_tx = users[1].1.clone();

    let mut alice_events: Vec<SessionEvent> = Vec::new();
    let mut bob_events: Vec<SessionEvent> = Vec::new();

    // File a Deadlock ECP. With both members as stewards and bob's
    // auto-vote, the emergency proposal reaches consensus YES.
    let current_epoch = alice_session.read().unwrap().epoch_and_retry().unwrap().0;
    let request = ViolationEvidence::deadlock(current_epoch)
        .with_creator(b"alice-creator".to_vec())
        .into_update_request()
        .unwrap();
    alice_session
        .write()
        .unwrap()
        .initiate_proposal(request, CreatorVote::Yes)
        .unwrap();

    let mut alice_saw_freezing = false;
    let mut bob_saw_freezing = false;
    for _ in 0..30 {
        settle_for(Duration::from_millis(40));
        poll_once(&alice_session);
        poll_once(&bob_session);
        flush_session(&alice_session, &alice_tx);
        flush_session(&bob_session, &bob_tx);
        let packets = alice_tx.lock().unwrap().drain_packets();
        for p in packets {
            deliver(&users[1].0, &p);
        }
        let packets = bob_tx.lock().unwrap().drain_packets();
        for p in packets {
            deliver(&users[0].0, &p);
        }

        alice_events.extend(alice_session.read().unwrap().drain_events());
        bob_events.extend(bob_session.read().unwrap().drain_events());
        if alice_events
            .iter()
            .any(|e| matches!(e, SessionEvent::PhaseChange(s) if *s == ConversationState::Freezing))
        {
            alice_saw_freezing = true;
        }
        if bob_events
            .iter()
            .any(|e| matches!(e, SessionEvent::PhaseChange(s) if *s == ConversationState::Freezing))
        {
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
