//! Steward behaviour: the inactivity timer auto-commits approved work with no
//! explicit commit call, and the settled-member rule keeps small / freshly
//! grown groups from firing a steward election (no election storm).

mod common;

use std::time::Duration;

use common::harness::{TestHarness, fast_config};
use de_mls::{ConversationConfig, StewardListConfig};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

#[test]
fn steward_auto_commits_approved_work_on_inactivity() {
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "b1",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    assert!(h.member(0).is_steward(), "creator is the steward");
    assert_eq!(h.epoch(), 1, "the bob-add commit advanced epoch 0 -> 1");

    // File a removal, then never ask for a commit: the steward's commit
    // inactivity timer (driven by `poll`) must build, vote-resolve, and merge
    // the removal on its own.
    let bob_id = h.member(1).member_id_bytes().to_vec();
    h.member_mut(0).remove_member(&bob_id);

    h.process_until("steward auto-commits the removal", |h| {
        h.member(0).member_count() == 1
    });

    assert_eq!(h.member(0).epoch(), 2, "the removal commit advanced 1 -> 2");
    assert!(h.member(1).saw_leaving(), "bob observes its own removal");
}

#[test]
fn small_group_reconciles_locally_without_election() {
    // n = 2, sn_max = 3 → the steward list fits; reconciled locally, no vote.
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "sg",
        fast_config(),
        StewardListConfig::new(1, 3).unwrap(),
    );
    drive_past_deadlines(&mut h);
    assert_no_election_storm(&h);
}

#[test]
fn over_sn_max_but_unsettled_members_do_not_elect() {
    // n = 3, sn_max = 2 → more members than sn_max, but the joiners aren't
    // settled this epoch, so the settled set still fits: no premature election.
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "lg",
        fast_config(),
        StewardListConfig::new(1, 2).unwrap(),
    );
    drive_past_deadlines(&mut h);
    assert_no_election_storm(&h);
}

fn drive_past_deadlines<const N: usize>(h: &mut TestHarness<N>) {
    for _ in 0..15 {
        h.process(Duration::from_millis(40));
    }
}

fn assert_no_election_storm<const N: usize>(h: &TestHarness<N>) {
    assert!(h.all_working(), "every member stays Working");
    assert!(h.epochs_agree(), "every member agrees on the epoch");
    assert!(
        h.member(0).is_steward(),
        "a committer always exists (creator stays a steward)"
    );
    assert!(
        h.members().iter().all(|m| m.retry_round() == 0),
        "retry_round stays 0 — no election fires for unsettled members"
    );
}

#[test]
fn backup_steward_proposes_buffered_joiner_when_epoch_steward_silent() {
    // sn_max = 5 ≥ membership at every size here, so the steward list is always
    // the full roster and growing back to 3 fires no subset election — the test
    // isolates the backup *proposal* takeover, not election.
    let cfg = ConversationConfig {
        recovery_inactivity_duration: Duration::from_millis(100),
        ..fast_config()
    };
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "bk",
        cfg.clone(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    // Evict member 2 so we can re-announce it as a fresh joiner.
    let target = 2usize;
    let target_id = h.member(target).member_id_bytes().to_vec();
    let driver = (0..3).find(|&i| i != target).unwrap();
    h.member_mut(driver).remove_member(&target_id);
    h.process_until("target evicted", |h| h.member(driver).member_count() == 2);

    // Identify the two remaining stewards' roles.
    let epoch_steward = (0..3)
        .filter(|&i| i != target)
        .find(|&i| h.member(i).convo().is_epoch_steward().unwrap())
        .expect("an epoch steward exists among the remaining two");
    let backup = (0..3)
        .find(|&i| i != target && i != epoch_steward)
        .expect("a backup steward exists");

    // The joiner announces, but only the backup observes it — the epoch steward
    // never sees the announcement, so it never sponsors. Without the backup
    // takeover the join would stall forever; with it, the backup proposes the
    // buffered Add after the recovery window and the live epoch steward commits.
    h.member_mut(target).rejoin("bk", cfg.clone());
    let announcement = h.member_mut(target).announce_key_package("bk");
    h.deliver_key_package_to(backup, &announcement);

    h.process_until("backup carries the join to completion", |h| {
        h.member(backup).member_count() == 3 && h.member(target).member_count() == 3
    });
}
