//! Welcome broadcast: when a commit adds a member, every group member
//! surfaces `ConversationEvent::WelcomeReady` — the committing steward
//! mints it (`minted_locally == true`) and broadcasts it in-group, peers
//! receive the same welcome with `minted_locally == false`. Duplicate
//! broadcast deliveries emit a single event, and the joiner can complete
//! the join from a welcome captured on any member.

use std::time::Duration;

use de_mls::core::{ConversationEvent, ConversationState, StewardListConfig};
use de_mls::protos::de_mls::messages::v1::MemberWelcome;

mod common;
use common::conversation_fixtures::{
    ConversationArc, bootstrap_joined_conversation, deliver, fast_test_config, flush_conversation,
    make_user, poll_once, predicate, settle_for,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

/// Drain a session's events and keep only the `WelcomeReady` ones.
fn drain_welcomes(session: &ConversationArc) -> Vec<(MemberWelcome, bool)> {
    session
        .read()
        .unwrap()
        .drain_events()
        .into_iter()
        .filter_map(|e| match e {
            ConversationEvent::WelcomeReady {
                welcome,
                minted_locally,
            } => Some((welcome, minted_locally)),
            _ => None,
        })
        .collect()
}

#[test]
fn welcome_broadcast_reaches_every_member_and_dedupes() {
    let conversation = "welcome-broadcast";
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        conversation,
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    let alice_session = users[0].0.lookup_entry(conversation).unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry(conversation).unwrap().unwrap();
    let sessions = [&alice_session, &bob_session];

    // Charlie mints a KP out of band; Bob — a non-creator — proposes the add.
    let (mut charlie, _charlie_transport) = make_user(
        CHARLIE,
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    let charlie_kp = charlie.generate_key_package().unwrap();
    bob_session
        .write()
        .unwrap()
        .add_member(charlie_kp.as_bytes())
        .unwrap();

    // Drive both members until each surfaces a WelcomeReady; capture the
    // in-group broadcast packet on the way for the dedup check below.
    let mut welcomes: Vec<Vec<(MemberWelcome, bool)>> = vec![Vec::new(), Vec::new()];
    let mut broadcast_packets = Vec::new();
    for _ in 0..40 {
        settle_for(Duration::from_millis(60));
        for (i, s) in sessions.iter().enumerate() {
            poll_once(s);
            flush_conversation(s, &users[i].1);
        }
        let mut packets = Vec::new();
        for (_, h) in &users {
            packets.extend(h.lock().unwrap().drain_packets());
        }
        for p in &packets {
            if predicate::is_member_welcome(p) {
                broadcast_packets.push(p.clone());
            }
            for (u, _) in &users {
                deliver(u, p);
            }
        }
        for (i, s) in sessions.iter().enumerate() {
            welcomes[i].extend(drain_welcomes(s));
        }
        if welcomes.iter().all(|w| !w.is_empty()) {
            break;
        }
    }

    assert!(
        welcomes.iter().all(|w| w.len() == 1),
        "each member surfaces exactly one WelcomeReady, got {:?}",
        welcomes.iter().map(|w| w.len()).collect::<Vec<_>>()
    );
    assert!(
        !broadcast_packets.is_empty(),
        "the committer broadcasts the welcome in-group"
    );

    let minted_count = welcomes.iter().filter(|w| w[0].1).count();
    assert_eq!(
        minted_count, 1,
        "exactly one member (the committing steward) mints the welcome"
    );
    assert_eq!(
        welcomes[0][0].0.welcome_bytes, welcomes[1][0].0.welcome_bytes,
        "committer and peer surface the same welcome bytes"
    );

    // Re-delivering the broadcast to the non-committer is a no-op: the
    // welcome hash is already recorded, so no second event fires.
    let non_committer = if welcomes[0][0].1 { 1 } else { 0 };
    for p in &broadcast_packets {
        deliver(&users[non_committer].0, p);
    }
    assert!(
        drain_welcomes(sessions[non_committer]).is_empty(),
        "duplicate broadcast must not re-emit WelcomeReady"
    );

    // Any member's copy completes the joiner: hand the non-committer's
    // welcome to Charlie.
    let (welcome, _) = &welcomes[non_committer][0];
    assert_eq!(welcome.joiner_identities.len(), 1);
    let joined = charlie.accept_welcome(welcome).expect("charlie joins");
    assert_eq!(joined, conversation);
    let charlie_session = charlie.lookup_entry(conversation).unwrap().unwrap();
    assert_eq!(
        charlie_session.read().unwrap().state(),
        ConversationState::Working
    );
}
