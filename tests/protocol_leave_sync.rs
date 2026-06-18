//! Self-leave idempotency and ConversationSync idempotency — two "do it twice,
//! the second is a no-op" guarantees the protocol must hold.

mod common;

use common::harness::{TestHarness, fast_config};
use de_mls::ConversationConfig;
use de_mls::StewardListConfig;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[test]
fn double_self_leave_does_not_re_propose() {
    let mut h = TestHarness::<1>::start(
        [ALICE],
        "c4",
        ConversationConfig::default(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    // Discard whatever group creation buffered.
    h.member_mut(0).take_outbound();

    h.member_mut(0).leave();
    let first = h.member_mut(0).take_outbound().len();
    assert!(first > 0, "first leave files a self-leave proposal");

    h.member_mut(0).leave();
    let second = h.member_mut(0).take_outbound().len();
    assert_eq!(second, 0, "second leave dedups — no new outbound");
}

#[test]
fn second_conversation_sync_is_a_no_op() {
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "c2",
        fast_config(),
        StewardListConfig::new(1, 3).unwrap(),
    );

    // The exact (encrypted) sync bob applied on join; replaying it is the test.
    let sync = h
        .member(1)
        .last_welcome_sync()
        .expect("bob joined with a ConversationSync")
        .to_vec();
    let alice_id = h.member(0).app_id().to_vec();

    let roles_before = h.member(1).member_roles();
    let scores_before = h.member(1).member_scores();
    h.member_mut(1).take_outbound(); // clear

    h.member_mut(1).deliver_raw(&alice_id, &sync);

    assert!(
        h.member_mut(1).take_outbound().is_empty(),
        "a second sync must not emit any outbound"
    );
    assert_eq!(
        roles_before,
        h.member(1).member_roles(),
        "a second sync must not change roles"
    );
    assert_eq!(
        scores_before,
        h.member(1).member_scores(),
        "a second sync must not change scores"
    );
}
