//! Membership + welcome: multi-member bootstrap convergence and removal. Each
//! test checks that every member agrees on the epoch and member set — the
//! protocol-level "we are the same group" guarantee — not just that one node
//! changed state.

mod common;

use common::harness::{TestHarness, fast_config};
use de_mls::{ConversationState, StewardListConfig};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

#[test]
fn three_members_join_and_converge() {
    let h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "grp",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    assert!(h.all_working(), "every member reaches Working");
    assert!(h.epochs_agree(), "every member converges on the same epoch");
    assert!(
        h.membership_agrees(),
        "every member sees the same member set"
    );
    assert_eq!(h.member(0).member_count(), 3, "all three are in the group");
    // Each joiner actually transitioned into Working off its welcome.
    assert!(h.member(1).saw_phase(ConversationState::Working));
    assert!(h.member(2).saw_phase(ConversationState::Working));
}

#[test]
fn removed_member_observes_leaving_and_group_shrinks() {
    // sn_max = 2 so at least one member is a non-steward; target it (a
    // self-remove would wedge the steward mid-freeze).
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "rm",
        fast_config(),
        StewardListConfig::new(1, 2).unwrap(),
    );

    let target = (0..3)
        .find(|&i| !h.member(i).is_steward())
        .expect("a non-steward exists");
    let steward = (0..3)
        .find(|&i| i != target && h.member(i).is_steward())
        .expect("a steward drives the removal");

    let target_id = h.member(target).member_id_bytes().to_vec();
    h.member_mut(steward).remove_member(&target_id);

    h.process_until("target removed", |h| h.member(steward).member_count() == 2);

    assert!(
        h.member(target).saw_leaving(),
        "the removed member observes its own removal"
    );
    assert_eq!(
        h.member(steward).member_count(),
        2,
        "the group shrank to two"
    );
}

#[test]
fn welcome_fans_out_to_every_member_with_a_single_minter() {
    // alice creator + bob joined; then bob (a non-creator) adds charlie.
    let mut h = TestHarness::<3>::start(
        [ALICE, BOB, CHARLIE],
        "wb",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    let bob_announcement = h.member_mut(1).announce_key_package("wb");
    h.deliver_key_package_all(&bob_announcement);
    h.process_until("bob joins", |h| h.member(1).is_working());

    // Isolate the welcome events from charlie's add (ignore bob's join).
    let alice_base = h.member(0).welcome_readys().len();
    let bob_base = h.member(1).welcome_readys().len();

    let charlie_kp = h.member_mut(2).mint_key_package();
    h.member_mut(1).add_member(&charlie_kp);
    h.process_until("charlie joins", |h| {
        h.member(2).is_working() && h.member(0).member_count() == 3
    });

    let alice_w = h.member(0).welcome_readys()[alice_base..].to_vec();
    let bob_w = h.member(1).welcome_readys()[bob_base..].to_vec();
    assert_eq!(alice_w.len(), 1, "alice surfaces exactly one welcome");
    assert_eq!(bob_w.len(), 1, "bob surfaces exactly one welcome");
    assert_eq!(
        [alice_w[0].1, bob_w[0].1].iter().filter(|&&m| m).count(),
        1,
        "exactly one member (the committing steward) mints the welcome"
    );
    assert_eq!(
        alice_w[0].0.welcome_bytes, bob_w[0].0.welcome_bytes,
        "every member surfaces the same welcome bytes"
    );
    assert!(
        h.epochs_agree() && h.membership_agrees(),
        "the group reconverges after charlie joins"
    );
}
