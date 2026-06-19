//! Freeze coordination: a commit cycle walks Freezing → Selection →
//! CommitApplied → Working; a silent steward drives an observer into
//! Reelection; and a Deadlock ECP force-freezes the whole group.

mod common;

use common::harness::{TestHarness, fast_config};
use de_mls::CreatorVote;
use de_mls::protos::de_mls::messages::v1::ViolationEvidence;
use de_mls::{ConversationState, StewardListConfig};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[test]
fn freeze_cycle_emits_phases_in_order() {
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "c3",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    // Only look at phases emitted from here on (ignore the join cycle's).
    let baseline = h.member(0).events().len();
    let bob_id = h.member(1).member_id_bytes().to_vec();
    h.member_mut(0).remove_member(&bob_id);

    h.process_until("steward completes the freeze cycle", |h| {
        h.member(0).member_count() == 1 && h.member(0).is_working()
    });

    let phases: Vec<ConversationState> = h.member(0).events()[baseline..]
        .iter()
        .filter_map(|e| match e {
            de_mls::ConversationEvent::PhaseChange(s) => Some(*s),
            _ => None,
        })
        .collect();
    assert!(
        is_subsequence(
            &phases,
            &[
                ConversationState::Freezing,
                ConversationState::Selection,
                ConversationState::Working,
            ],
        ),
        "steward must walk Freezing → Selection → Working, got {phases:?}"
    );
    assert!(
        h.member(0).commits_applied() >= 1,
        "the freeze cycle applies a commit"
    );
}

#[test]
fn silent_steward_drives_observer_to_reelection() {
    // sn_max = 1 → exactly one steward; the other member is the observer.
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "b2",
        fast_config(),
        StewardListConfig::new(1, 1).unwrap(),
    );
    let (steward, observer) = if h.member(0).is_steward() {
        (0, 1)
    } else {
        (1, 0)
    };

    // The observer files approved work, then the steward goes silent: the
    // observer can't author a candidate itself and never sees the steward's,
    // so its freeze window elapses into Reelection.
    let steward_id = h.member(steward).member_id_bytes().to_vec();
    h.member_mut(observer).remove_member(&steward_id);
    h.process_until("observer has approved work", |h| {
        h.member(observer).approved_count() > 0
    });

    h.mute(steward);
    h.process_until("observer enters Reelection", |h| {
        h.member(observer).state() == ConversationState::Reelection
    });
}

#[test]
fn deadlock_ecp_force_freezes_the_group() {
    // sn_max = 2 with two members → both are stewards, so the emergency
    // proposal reaches consensus YES on the bundled + auto vote.
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "b5",
        fast_config(),
        StewardListConfig::new(2, 2).unwrap(),
    );

    let epoch = h.epoch();
    let ecp = ViolationEvidence::deadlock(epoch)
        .with_creator(b"alice-creator".to_vec())
        .into_update_request()
        .unwrap();
    h.member_mut(0).initiate_proposal(ecp, CreatorVote::Yes);

    // Reaching the predicate (process_until panics on timeout) is the assertion.
    h.process_until("both members force-freeze", |h| {
        h.member(0).saw_phase(ConversationState::Freezing)
            && h.member(1).saw_phase(ConversationState::Freezing)
    });
}

/// `needle` appears in `haystack` in order (intervening elements allowed).
fn is_subsequence(haystack: &[ConversationState], needle: &[ConversationState]) -> bool {
    let mut it = haystack.iter();
    needle.iter().all(|want| it.any(|got| got == want))
}
