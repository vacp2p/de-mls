//! Founding a group with members you already hold key packages for.
//!
//! These tests pin how the protocol behaves *today* when a creator seeds a
//! group with several friends at once, before any of them has run. They are
//! the baseline a "genesis" create-with-members API has to beat, and they
//! measure two things the rest of the suite never exercises:
//!
//! - what the founding adds cost in wall-clock time, on a config whose
//!   inactivity window is long enough to see (every other test runs
//!   `fast_config`, where the window is 50 ms and effectively invisible);
//! - what an invitee who is in the ratchet tree but has never opened its
//!   welcome does to consensus — `withhold_welcome` models exactly that.

mod common;

use std::time::Duration;

use common::harness::{TestHarness, fast_config};
use de_mls::{ConversationConfig, StewardListConfig};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const DAVE: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";
const ERIN: &str = "47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a";
const FRANK: &str = "8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba";

/// One driving step of virtual time.
const STEP: Duration = Duration::from_millis(50);

/// A config whose commit-inactivity window is long enough to measure, while
/// every other gate stays sub-second. Preserves the documented ordering
/// invariant `voting_delay < consensus_timeout < commit_inactivity_duration`.
fn measurable_commit_window() -> ConversationConfig {
    ConversationConfig {
        commit_inactivity_duration: Duration::from_secs(2),
        freeze_duration: Duration::from_millis(20),
        voting_delay: Duration::from_millis(30),
        consensus_timeout: Duration::from_millis(150),
        proposal_expiration: Duration::from_secs(10),
        ..ConversationConfig::default()
    }
}

/// Drive until `predicate` holds, returning the virtual time it took. Unlike
/// `process_until` this reports elapsed time rather than discarding it.
fn drive_for<const N: usize>(
    h: &mut TestHarness<N>,
    label: &str,
    budget: Duration,
    predicate: impl Fn(&TestHarness<N>) -> bool,
) -> Duration {
    let mut elapsed = Duration::ZERO;
    while !predicate(h) {
        assert!(elapsed < budget, "never converged: {label}");
        h.process(STEP);
        elapsed += STEP;
    }
    elapsed
}

/// The creator seeds three friends at epoch 0, before any of them has run.
/// Pins that they batch into a single commit and a single welcome — and that
/// the whole group still waits one full commit-inactivity window for a commit
/// a one-member group already unanimously agreed with itself about.
#[test]
fn founding_adds_batch_into_one_commit_after_one_inactivity_window() {
    let config = measurable_commit_window();
    let mut h = TestHarness::<4>::start(
        [ALICE, BOB, CHARLIE, DAVE],
        "found",
        config.clone(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    for i in 1..4 {
        let kp = h.member_mut(i).mint_key_package();
        h.member_mut(0).add_member(&kp);
    }
    assert_eq!(h.member(0).epoch(), 0, "nothing has committed yet");
    assert_eq!(h.member(0).member_count(), 1, "the creator is alone");

    let landed_at = drive_for(
        &mut h,
        "founding commit lands",
        Duration::from_secs(10),
        |h| h.member(0).commits_applied() == 1,
    );

    assert!(
        landed_at >= config.commit_inactivity_duration,
        "the founding commit waited a full inactivity window ({:?}), landed at {landed_at:?}",
        config.commit_inactivity_duration,
    );

    let welcomes = h.member(0).welcome_readys();
    assert_eq!(welcomes.len(), 1, "all three adds mint exactly one welcome");
    assert_eq!(
        welcomes[0].0.joiner_identities.len(),
        3,
        "the single welcome addresses all three friends"
    );

    drive_for(
        &mut h,
        "friends open their welcomes",
        Duration::from_secs(10),
        |h| h.all_working(),
    );

    assert_eq!(h.member(0).epoch(), 1, "one commit, one epoch");
    assert_eq!(h.member(0).member_count(), 4, "the group is whole");
    assert_eq!(
        h.member(0).commits_applied(),
        1,
        "three adds cost exactly one commit"
    );
    assert!(h.epochs_agree() && h.membership_agrees());
}

/// Immediately after the founding commit, the freshly-added friends are not
/// yet settled, so the creator remains the sole steward for the whole of
/// epoch 1. Pins the property a genesis path would change by marking founders
/// settled — and shows the creator's epoch-1 stewardship is a *consequence* of
/// the settled rule, not a guarantee of creation.
#[test]
fn founders_are_not_stewards_in_the_epoch_they_arrive() {
    let mut h = TestHarness::<4>::start(
        [ALICE, BOB, CHARLIE, DAVE],
        "settle",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    for i in 1..4 {
        let kp = h.member_mut(i).mint_key_package();
        h.member_mut(0).add_member(&kp);
    }
    drive_for(
        &mut h,
        "founding commit lands",
        Duration::from_secs(10),
        |h| h.all_working() && h.member(0).member_count() == 4,
    );

    assert_eq!(h.member(0).epoch(), 1);
    assert!(
        h.member(0).is_epoch_steward(),
        "the creator stewards epoch 1 alone"
    );
    for i in 1..4 {
        assert!(
            !h.member(i).is_steward(),
            "member {i} joined at epoch 1 and is not settled, so not a steward"
        );
    }
}

/// Five members in the tree, four of whom have never run. The creator adds a
/// sixth. Pins what the four silent leaves contribute to that vote.
#[test]
fn silent_founders_are_counted_as_voters_on_the_next_add() {
    let mut h = TestHarness::<6>::start(
        [ALICE, BOB, CHARLIE, DAVE, ERIN, FRANK],
        "ghost",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    // Four friends land in the tree but never open their welcome.
    for i in 1..5 {
        h.member_mut(i).withhold_welcome();
        let kp = h.member_mut(i).mint_key_package();
        h.member_mut(0).add_member(&kp);
    }
    drive_for(
        &mut h,
        "founding commit lands",
        Duration::from_secs(10),
        |h| h.member(0).member_count() == 5,
    );

    for i in 1..5 {
        assert!(!h.member(i).is_working(), "member {i} never ran");
    }
    assert!(
        h.member(0).is_epoch_steward(),
        "the creator is the only member who can commit"
    );

    // Frank is a real joiner: the creator proposes him with four of five
    // expected voters permanently silent.
    let frank_kp = h.member_mut(5).mint_key_package();
    h.member_mut(0).add_member(&frank_kp);

    let outcome = drive_for(
        &mut h,
        "frank's add resolves either way",
        Duration::from_secs(10),
        |h| h.member(5).is_working() || h.member(0).epoch() > 1,
    );

    assert!(
        h.member(5).is_working(),
        "frank joined at {outcome:?} — four members who never ran consented for him"
    );
    assert_eq!(h.member(0).member_count(), 6);
}

/// The two-person case: me and one friend who never opens the welcome. Pins
/// what the pair can still do — the most common real-world founding shape.
#[test]
fn a_single_silent_founder_and_the_next_add() {
    let mut h = TestHarness::<3>::start(
        [ALICE, BOB, CHARLIE],
        "pair",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    h.member_mut(1).withhold_welcome();
    let bob_kp = h.member_mut(1).mint_key_package();
    h.member_mut(0).add_member(&bob_kp);
    drive_for(
        &mut h,
        "bob lands in the tree",
        Duration::from_secs(10),
        |h| h.member(0).member_count() == 2,
    );
    assert!(!h.member(1).is_working(), "bob never opened his welcome");

    let charlie_kp = h.member_mut(2).mint_key_package();
    h.member_mut(0).add_member(&charlie_kp);

    // Drive well past consensus_timeout so a timeout resolution has landed.
    let mut elapsed = Duration::ZERO;
    while elapsed < Duration::from_secs(3) {
        h.process(STEP);
        elapsed += STEP;
    }

    assert_eq!(
        h.member(0).member_count(),
        2,
        "charlie never lands: one silent member of two vetoes by silence"
    );
    assert!(!h.member(2).is_working());
}

/// A send-only partition of the sole epoch-1 steward: it still hears proposals,
/// so it still mints commits, but nothing it sends arrives.
///
/// The guarantee this test pins is recovery — the survivors land the add
/// without the creator, so a group does not die with its founder.
///
/// Known gap, documented but not asserted: the isolated creator merged its own
/// lost commit and advanced onto a private branch. Both branches report the
/// same epoch number and member set — `epochs_agree`/`membership_agrees` can't
/// tell them apart — and nothing detects the divergence. We assert the recovery
/// and that integer-level agreement (which is *why* the fork is silent), but
/// deliberately not that the fork persists: a future fork fix shouldn't have to
/// fight this test.
#[test]
fn survivors_recover_when_the_sole_steward_is_partitioned() {
    let mut h = TestHarness::<5>::start(
        [ALICE, BOB, CHARLIE, DAVE, ERIN],
        "offline",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    for i in 1..4 {
        let kp = h.member_mut(i).mint_key_package();
        h.member_mut(0).add_member(&kp);
    }
    drive_for(
        &mut h,
        "founding commit lands",
        Duration::from_secs(10),
        |h| h.member(0).member_count() == 4 && h.members()[..4].iter().all(|m| m.is_working()),
    );
    assert_eq!(h.member(0).epoch(), 1);
    assert!(h.member(0).is_epoch_steward(), "creator stewards epoch 1");
    assert!(
        h.converged(),
        "the founded group is one group to begin with"
    );

    h.mute(0);

    let erin_kp = h.member_mut(4).mint_key_package();
    h.member_mut(1).add_member(&erin_kp);

    let recovered_at = drive_for(
        &mut h,
        "the survivors land a commit without their creator",
        Duration::from_secs(25),
        |h| h.member(1).member_count() == 5,
    );
    assert!(
        h.member(1).epoch() > 1,
        "the survivors advanced the epoch on their own at {recovered_at:?}"
    );
    assert!(h.member(4).is_working(), "erin joined without the creator");

    // The creator returns from the partition. Nothing repairs the divergence:
    // its branch and the survivors' report the same epoch number and member set
    // even though their epoch secrets differ — which is exactly why the fork
    // was silent before we surfaced the authenticator.
    h.unmute(0);
    for _ in 0..40 {
        h.process(STEP);
    }

    assert!(
        h.epochs_agree() && h.membership_agrees(),
        "the integer views match — the reason the fork went unnoticed"
    );
    // The capability this slice adds: the authenticator catches the divergence
    // the integer checks miss. This assertion is the fork's canary — when
    // prevention or auto-rejoin lands, this scenario stops forking and the
    // assertion flips to `converged()`. That break is the signal to update the
    // test, not a regression.
    assert!(
        !h.converged(),
        "the epoch authenticators diverge — the fork the counts can't see"
    );
}
