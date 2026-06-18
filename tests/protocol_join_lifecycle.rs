//! Join-lifecycle edge flows: an evicted member can rejoin at a strictly later
//! epoch.

mod common;

use common::harness::{TestHarness, fast_config};
use de_mls::StewardListConfig;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

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
