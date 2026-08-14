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
const DAVE: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";
const ERIN: &str = "47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a";

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

/// A merged add surfaces `MembersChanged` so the integrator learns the new
/// member's leaf index — the id it tracks the member by — and can map it onto
/// its own identity via the carried credential.
#[test]
fn add_emits_members_changed_naming_the_new_leaf() {
    use de_mls::ConversationEvent;
    let mut h = TestHarness::<2>::start(
        [ALICE, BOB],
        "mc",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    let bob_credential = h.member(1).credential_id().to_vec();
    let announcement = h.member_mut(1).announce_key_package("mc");
    h.deliver_key_package_all(&announcement);
    h.process_until("bob joins", |h| h.member(1).is_working());

    // The creator observed the add commit and emitted MembersChanged, naming
    // bob by his credential at his assigned leaf.
    let bob_add = h
        .member(0)
        .events()
        .iter()
        .filter_map(|e| match e {
            ConversationEvent::MembersChanged { added, .. } => Some(added),
            _ => None,
        })
        .flatten()
        .find(|m| m.credential.serialized_content() == bob_credential)
        .expect("MembersChanged names bob by credential")
        .clone();

    // The reported leaf index, encoded, is exactly bob's own de-mls member_id.
    assert_eq!(
        bob_add.index.u32().to_be_bytes().to_vec(),
        h.member(1).member_id_bytes().to_vec(),
        "the leaf index in the event is bob's member_id"
    );
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

/// A plain member inviting someone with `add_member` must cost exactly one
/// proposal.
///
/// `add_member` is an invitation — the caller already holds the joiner's key
/// package — and is distinct from `sponsor_member`, which relays a join request
/// announced on the group's channel. But every member mirrors each membership
/// proposal it sees into `pending_updates`, the backup buffer whose job is to
/// retry an add that never landed. The epoch steward drains that buffer
/// immediately, and the drain skips only targets that already carry an
/// *approved* proposal — not one still in flight. So the steward re-proposes an
/// invitation that is already being voted on, doubling the consensus traffic and
/// putting two Adds for one member into the commit batch.
///
/// Four members is the smallest group that shows it, and the size is load-bearing:
/// `poll` runs `tick_deadlines` before the buffer drain, so at three members the
/// steward's own auto-vote completes the `ceil(2n/3) = 2` quorum inside the same
/// poll and the drain correctly sees an approved proposal. From four members up,
/// quorum needs a vote that has not been relayed yet, the drain sees nothing
/// approved, and it re-proposes.
#[test]
fn non_steward_add_member_costs_exactly_one_proposal() {
    let mut h = TestHarness::<5>::start(
        [ALICE, BOB, CHARLIE, DAVE, ERIN],
        "one-prop",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    for i in 1..4 {
        let kp = h.member_mut(i).mint_key_package();
        h.member_mut(0).add_member(&kp);
    }
    h.process_until("founding commit lands", |h| {
        h.member(0).member_count() == 4 && h.members()[..4].iter().all(|m| m.is_working())
    });
    assert!(
        !h.member(1).is_steward(),
        "bob must be a plain member for this to test the non-steward path"
    );

    let erin_kp = h.member_mut(4).mint_key_package();
    let erin_id = erin_kp.member_id().to_vec();
    h.member_mut(1).add_member(&erin_kp);
    h.process_until("erin joins", |h| h.member(4).is_working());

    for i in 0..4 {
        let seen = h.member(i).observed_invite_proposals(&erin_id).len();
        assert_eq!(
            seen, 1,
            "member {i} saw {seen} proposals for a single invitation"
        );
    }
    assert_eq!(
        h.member(0)
            .committed_adds()
            .iter()
            .filter(|id| **id == erin_id)
            .count(),
        1,
        "erin is added exactly once"
    );
}

/// An invitation and a sponsored join request can name the same member at the
/// same time: bob holds erin's key package and invites her (RFC step 5, "any
/// member start to create voting proposals"), while erin also publishes a key
/// package that the steward turns into its own proposal (RFC step 3, "upon
/// receipt, the steward MUST initiate a voting proposal"). Neither node can see
/// the other's proposal before creating its own, so both reach consensus.
///
/// The two invites carry different key packages, so — unlike two duplicate
/// removals, which are byte-identical and collapse into one MLS proposal — they
/// become two Adds for one leaf, and MLS rejects the whole commit with
/// "Duplicate signature key in proposals and group". The steward retries and
/// fails identically every round, so the group stops admitting anyone at all.
#[test]
fn invitation_racing_a_sponsored_join_admits_the_member_once() {
    let mut h = TestHarness::<5>::start(
        [ALICE, BOB, CHARLIE, DAVE, ERIN],
        "race",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    for i in 1..4 {
        let kp = h.member_mut(i).mint_key_package();
        h.member_mut(0).add_member(&kp);
    }
    h.process_until("founding commit lands", |h| {
        h.member(0).member_count() == 4 && h.members()[..4].iter().all(|m| m.is_working())
    });
    let epoch_before = h.member(0).epoch();

    // Two key packages for erin, proposed by two different members.
    let invited_kp = h.member_mut(4).mint_key_package();
    let erin_id = invited_kp.member_id().to_vec();
    let announced_kp = h.member_mut(4).announce_key_package("race");

    h.deliver_key_package_to(0, &announced_kp); // steward sponsors the request
    h.member_mut(1).add_member(&invited_kp); // bob invites her in parallel

    h.process_until("erin joins despite the racing invites", |h| {
        h.member(4).is_working()
    });

    assert_eq!(
        h.member(0)
            .committed_adds()
            .iter()
            .filter(|id| **id == erin_id)
            .count(),
        1,
        "erin is admitted exactly once"
    );
    assert_eq!(h.member(0).member_count(), 5, "the group grew by one");
    assert!(
        h.member(0).epoch() > epoch_before,
        "the round committed rather than stalling"
    );
    assert!(h.epochs_agree() && h.membership_agrees());
    assert!(h.converged(), "the group stays one group");
}

/// The same node proposing one joiner twice — with different key packages, so
/// the two requests don't collide on their content-derived proposal id — must
/// open exactly one consensus session. The local in-flight dedup at proposal
/// initiation catches the second; without it, a redundant session runs and is
/// only cleaned up at approval.
#[test]
fn repeated_add_for_one_joiner_opens_one_proposal() {
    let mut h = TestHarness::<5>::start(
        [ALICE, BOB, CHARLIE, DAVE, ERIN],
        "double-add",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    for i in 1..4 {
        let kp = h.member_mut(i).mint_key_package();
        h.member_mut(0).add_member(&kp);
    }
    h.process_until("founding commit lands", |h| {
        h.member(0).member_count() == 4 && h.members()[..4].iter().all(|m| m.is_working())
    });

    let erin_kp_a = h.member_mut(4).mint_key_package();
    let erin_id = erin_kp_a.member_id().to_vec();
    let erin_kp_b = h.member_mut(4).mint_key_package();

    // Two proposals for erin, back to back, before any poll resolves the first.
    h.member_mut(0).add_member(&erin_kp_a);
    h.member_mut(0).add_member(&erin_kp_b);

    h.process_until("erin joins", |h| h.member(4).is_working());

    assert_eq!(
        h.member(0).observed_invite_proposals(&erin_id).len(),
        1,
        "a repeated add for one joiner opens exactly one proposal"
    );
    assert_eq!(
        h.member(0)
            .committed_adds()
            .iter()
            .filter(|id| **id == erin_id)
            .count(),
        1,
        "erin is added once"
    );
}
