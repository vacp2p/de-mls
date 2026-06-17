//! Join-lifecycle edge flows: a joiner whose welcome never arrives expires out
//! of `PendingJoin`, and an evicted member can rejoin at a strictly later
//! epoch.

mod common;

use std::time::Duration;

use common::harness::{Member, TestHarness, fast_config};
use de_mls::core::{ConversationState, StewardListConfig};
use de_mls::session::ConversationConfig;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

#[test]
fn pending_join_without_welcome_expires() {
    // A lone joiner for a conversation no creator is on: no welcome ever
    // arrives, so after 3× the commit-inactivity window the joiner gives up.
    let inactivity = Duration::from_millis(80);
    let cfg = ConversationConfig {
        commit_inactivity_duration: inactivity,
        ..ConversationConfig::default()
    };
    let mut joiner = Member::join(ALICE, "ghost", cfg, StewardListConfig::new(1, 5).unwrap());
    assert_eq!(joiner.state(), ConversationState::PendingJoin);

    // Poll until the pending-join window elapses (allow generous slack for CI).
    let mut leave_requested = false;
    for _ in 0..24 {
        std::thread::sleep(inactivity / 2);
        if joiner.poll().leave_requested {
            leave_requested = true;
            break;
        }
    }
    assert!(
        leave_requested,
        "pending-join must signal leave_requested after 3× inactivity"
    );

    joiner.pump_events();
    assert!(
        joiner.saw_leaving(),
        "the joiner must emit Leaving on pending-join expiry"
    );
}

#[test]
fn evicted_member_rejoins_at_a_later_epoch() {
    // sn_max = 2 → a non-steward exists; remove it, then rejoin it.
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "rejoin",
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
    let pre_remove_epoch = h.member(steward).epoch();

    h.member_mut(steward).remove_member(&target_id);
    h.process_until("target removed", |h| h.member(steward).member_count() == 2);

    // Rejoin: same identity registers afresh, announces a new key package, and
    // is driven back to Working.
    h.member_mut(target).rejoin("rejoin", fast_config());
    let announcement = h.member_mut(target).announce_key_package("rejoin");
    h.deliver_key_package_all(&announcement);
    h.process_until("target rejoins", |h| {
        h.member(target).is_working() && h.member(steward).member_count() == 3
    });

    assert!(
        h.member(target).epoch() > pre_remove_epoch,
        "rejoin lands at a strictly later epoch ({} > {pre_remove_epoch})",
        h.member(target).epoch()
    );
    assert!(
        h.member(steward)
            .convo()
            .members()
            .unwrap()
            .iter()
            .any(|m| m == &target_id),
        "the steward sees the rejoined identity back in the member set"
    );
}
