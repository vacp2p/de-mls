//! Application-message delivery through the protocol: a chat sent by one
//! member surfaces on a peer as a decrypted
//! `ConversationEvent::ConversationMessage`.

mod common;

use common::harness::{TestHarness, fast_config};
use de_mls::{ConversationState, StewardListConfig};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

#[test]
fn chat_message_delivered_to_peer() {
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "chat",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    // Bootstrap converged: both members joined, agree on epoch (the add commit
    // advanced it past 0), and bob actually transitioned into Working.
    assert!(h.all_working());
    assert!(h.epochs_agree(), "members disagree on epoch after join");
    assert!(h.membership_agrees(), "members disagree on the member set");
    assert_eq!(h.epoch(), 1, "the bob-add commit advances epoch 0 -> 1");
    assert!(
        h.member(1).saw_phase(ConversationState::Working),
        "bob transitioned into Working"
    );

    h.member_mut(0).send_message(b"Hello from alice".to_vec());
    h.process_until("bob receives chat", |h| {
        h.member(1).got_chat(b"Hello from alice")
    });

    // The message landed on the *other* member, decrypted, attributed to alice.
    let chat = &h.member(1).received()[0];
    assert_eq!(chat.body, b"Hello from alice");
    assert_eq!(chat.sender, h.member(0).member_id_bytes());
    // And the sender did not echo it back to itself.
    assert!(
        !h.member(0).got_chat(b"Hello from alice"),
        "sender must not receive its own chat"
    );
}

/// A receiving peer can resolve the MLS-authenticated signature key of a chat's
/// attributed sender via `member_signature_key(sender)`, and it matches the key
/// that sender actually signs with — the realistic sender-authentication path.
#[test]
fn received_chat_sender_signature_key_resolves_to_the_signer() {
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "sig-key",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    assert!(h.all_working());
    assert!(h.converged(), "group forked during bootstrap");

    h.member_mut(0).send_message(b"signed by alice".to_vec());
    h.process_until("bob and charlie receive alice's chat", |h| {
        h.member(1).got_chat(b"signed by alice") && h.member(2).got_chat(b"signed by alice")
    });

    let alice_key = h.member(0).signing_pubkey();
    // Each receiver reads the attributed sender and resolves its signing key,
    // landing on alice's real key.
    for receiver in [1usize, 2] {
        let chat = &h.member(receiver).received()[0];
        assert_eq!(chat.sender, h.member(0).member_id_bytes());
        assert_eq!(
            h.member(receiver).member_signature_key(&chat.sender),
            Some(alice_key.clone()),
            "receiver {receiver} resolved the wrong signing key for alice",
        );
    }
}

/// Every member resolves every member's signing key identically and correctly,
/// and an id belonging to no current member resolves to `None`.
#[test]
fn member_signature_key_agrees_across_the_group() {
    let h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "sig-key-all",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    assert!(h.all_working());
    assert!(h.membership_agrees(), "members disagree on the member set");

    for subject in 0..3 {
        let id = h.member(subject).member_id_bytes().to_vec();
        let expected = h.member(subject).signing_pubkey();
        for viewer in 0..3 {
            assert_eq!(
                h.member(viewer).member_signature_key(&id),
                Some(expected.clone()),
                "member {viewer} resolved the wrong signing key for member {subject}",
            );
        }
    }

    // An id that belongs to no current member has no key.
    assert_eq!(
        h.member(0).member_signature_key(b"stranger-not-in-group"),
        None,
        "a non-member id must not resolve to a signing key",
    );
}
