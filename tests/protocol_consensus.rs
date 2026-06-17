//! Negative consensus: a peer voting NO blocks the commit. Every other test
//! rides the bundled-YES / auto-vote happy path; this exercises the reject
//! branch — the proposal resolves `approved = false` and nothing changes.

mod common;

use std::time::Duration;

use common::harness::TestHarness;
use de_mls::core::StewardListConfig;
use de_mls::session::ConversationConfig;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// A voting delay long enough that a manual NO (cast within a polling round or
/// two) reliably beats the auto-vote. The commit-inactivity window is wide so a
/// pending joiner doesn't expire during the (slower) bootstrap, and the
/// consensus timeout is comfortable so the removal round resolves on the cast
/// votes rather than by timing out.
fn manual_vote_config() -> ConversationConfig {
    ConversationConfig {
        commit_inactivity_duration: Duration::from_secs(2),
        freeze_duration: Duration::from_millis(20),
        voting_delay: Duration::from_millis(300),
        election_voting_delay: Duration::from_millis(300),
        consensus_timeout: Duration::from_secs(5),
        proposal_expiration: Duration::from_secs(60),
        ..ConversationConfig::default()
    }
}

#[test]
fn peer_vote_no_blocks_the_commit() {
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "nc",
        manual_vote_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    let epoch_before = h.epoch();
    assert_eq!(epoch_before, 1, "bootstrap settled at epoch 1");

    // Alice proposes removing bob (her YES is bundled at submit).
    let bob_id = h.member(1).member_id_bytes().to_vec();
    h.member_mut(0).remove_member(&bob_id);

    // Bob surfaces the vote request and votes NO before any auto-vote fires.
    h.process_until("bob receives the vote request", |h| {
        h.member(1).pending_vote_request().is_some()
    });
    let proposal_id = h.member(1).pending_vote_request().unwrap();
    h.member_mut(1).vote(proposal_id, false);

    // With one YES and one NO across both expected voters, consensus resolves
    // to rejected.
    h.process_until("consensus rejects the proposal", |h| {
        h.member(1).consensus_outcome(proposal_id) == Some(false)
    });

    // Drive further to be sure nothing commits off the rejected proposal.
    for _ in 0..6 {
        h.process(Duration::from_millis(40));
    }

    assert_eq!(
        h.epoch(),
        epoch_before,
        "a rejected proposal must not advance the epoch"
    );
    assert!(h.epochs_agree(), "both members stay on the same epoch");
    assert_eq!(h.member(0).member_count(), 2, "bob is not removed");
    assert!(!h.member(1).saw_leaving(), "bob does not leave");
}
