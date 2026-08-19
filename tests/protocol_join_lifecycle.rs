//! Join-lifecycle edge flows: an evicted member can rejoin at a strictly later
//! epoch.

mod common;

use std::time::Duration;

use common::harness::{TestHarness, fast_config};
use de_mls::{ConversationConfig, ConversationEvent, StewardListConfig};

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

    let target_key = h.member(target).signing_pubkey();
    let pre_remove_epoch = h.member(steward).epoch();

    h.member_mut(steward).remove_member(&target_key);
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
        h.member(steward).member_id_for(&target_key).is_some(),
        "the steward sees the rejoined identity back in the member set"
    );
}

/// A joiner adopts the steward list from its welcome sync and reports the
/// healthy bootstrap — never the degraded `ConversationSyncMissing`.
#[test]
fn bootstrap_joiner_reports_sync_applied() {
    let h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "sync-applied",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    // Index 0 creates the group and builds the list itself; index 1 joins and
    // adopts the list from the welcome sync.
    let joiner = h.member(1);
    assert!(
        joiner
            .events()
            .iter()
            .any(|e| matches!(e, ConversationEvent::ConversationSyncApplied)),
        "joiner reports the sync it adopted from the welcome"
    );
    assert!(
        !joiner
            .events()
            .iter()
            .any(|e| matches!(e, ConversationEvent::ConversationSyncMissing)),
        "a healthy join is not degraded"
    );
}

/// A backup steward covers for a silent epoch steward: it re-sends the sync only
/// after `backup_takeover_window`, not before.
#[test]
fn backup_steward_resends_sync_when_epoch_steward_silent() {
    // sn_max = 5 → the full settled members is the steward list. Evicting one
    // member advances an epoch so the remaining joiners settle into stewards,
    // leaving an epoch steward and a backup.
    let cfg = ConversationConfig {
        backup_takeover_window: Duration::from_millis(100),
        ..fast_config()
    };
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "sync-takeover",
        cfg.clone(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    let target = 2usize;
    let target_id = h.member(target).signing_pubkey();
    let driver = (0..3).find(|&i| i != target).unwrap();
    h.member_mut(driver).remove_member(&target_id);
    h.process_until("target evicted", |h| h.member(driver).member_count() == 2);

    let epoch_steward = (0..3)
        .filter(|&i| i != target)
        .find(|&i| h.member(i).is_epoch_steward())
        .expect("an epoch steward exists");
    let backup = (0..3)
        .find(|&i| i != target && i != epoch_steward)
        .expect("a backup steward exists");

    // A request reaches the backup only; the epoch steward never answers. The
    // request's origin is irrelevant here — the test drives the backup takeover.
    h.member_mut(epoch_steward).take_outbound();
    h.member_mut(epoch_steward).request_conversation_sync();
    let request = h.member_mut(epoch_steward).take_outbound();
    for pkt in &request {
        h.member_mut(backup).deliver_raw(&pkt.sender, &pkt.payload);
    }

    // Within the window the backup holds off — the epoch steward owns the answer.
    h.member_mut(backup).take_outbound();
    h.member_mut(backup).poll();
    assert!(
        h.member_mut(backup).take_outbound().is_empty(),
        "backup waits out backup_takeover_window before covering"
    );

    // Past the window with no answer seen, the backup re-sends the sync.
    h.member(backup)
        .advance_clock(cfg.backup_takeover_window + Duration::from_millis(50));
    h.member_mut(backup).poll();
    assert_eq!(
        h.member_mut(backup).take_outbound().len(),
        1,
        "backup re-sends the sync once the epoch steward stays silent"
    );
}
