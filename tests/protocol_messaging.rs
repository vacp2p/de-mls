//! Application-message delivery through the protocol: a chat sent by one
//! member surfaces on a peer as a decrypted
//! `ConversationEvent::ConversationMessage`.

mod common;

use common::harness::{TestHarness, fast_config};
use de_mls::{ConversationState, StewardListConfig};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

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
