//! Asymmetric-clock reproduction of the failure that motivated the
//! caller-owned clock: a proposal that reaches a peer after its expiration
//! window (network latency exceeding the configured timers) is dropped by
//! that peer's consensus validation, so the peer never surfaces a vote
//! request. The control test pins the same flow on lockstep clocks, proving
//! the skew alone causes the drop.
//!
//! The skewed member's clock runs ahead of the proposer's, which is
//! equivalent to the proposal arriving late: by the time it lands, the
//! receiver's `now` is already past the proposal's `expiration_timestamp`.

mod common;

use std::time::Duration;

use common::harness::TestHarness;
use de_mls::ConversationConfig;
use de_mls::StewardListConfig;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// The proposal-validity window under test. The skew pushes the receiver
/// past it; the wire format is whole seconds, so both are multi-second.
const EXPIRATION: Duration = Duration::from_secs(2);
const SKEW: Duration = Duration::from_secs(3);

/// With two expected voters the consensus library requires both votes, so
/// the proposer's session fails at the timeout when the peer stays silent —
/// nothing commits over the peer that never saw the proposal.
fn skew_config() -> ConversationConfig {
    ConversationConfig {
        commit_inactivity_duration: Duration::from_secs(2),
        freeze_duration: Duration::from_millis(20),
        voting_delay: Duration::from_millis(300),
        consensus_timeout: Duration::from_millis(500),
        proposal_expiration: EXPIRATION,
        ..ConversationConfig::default()
    }
}

/// Control: on lockstep clocks the same proposal reaches the peer inside its
/// validity window and surfaces a vote request.
#[test]
fn lockstep_peer_surfaces_the_vote_request() {
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "skew-control",
        skew_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    assert!(
        h.member(1).pending_vote_request().is_none(),
        "no vote request exists before the removal is proposed"
    );

    let bob_id = h.member(1).member_id_bytes().to_vec();
    h.member_mut(0).remove_member(&bob_id);

    h.process_until("bob receives the vote request", |h| {
        h.member(1).pending_vote_request().is_some()
    });
}

/// Reproduction: the receiver's clock is past the proposal's expiration when
/// the proposal arrives, so consensus validation drops it — no vote request,
/// no session, and nothing commits on either side.
#[test]
fn proposal_arriving_past_expiration_is_dropped_by_skewed_peer() {
    let mut h = TestHarness::<2>::bootstrap(
        [ALICE, BOB],
        "skew-late",
        skew_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    let epoch_before = h.epoch();

    // Bob's clock runs ahead of alice's by more than the validity window —
    // every proposal alice stamps is already expired when bob validates it.
    h.member(1).advance_clock(SKEW);

    let bob_id = h.member(1).member_id_bytes().to_vec();
    h.member_mut(0).remove_member(&bob_id);

    // Drive well past the consensus timeout: the proposal round must resolve
    // (or retry) without bob ever seeing it.
    for _ in 0..30 {
        h.process(Duration::from_millis(50));
    }

    assert!(
        h.member(1).saw_expired_proposal(),
        "bob's inbound processing rejects the proposal as expired"
    );
    assert!(
        h.member(1).pending_vote_request().is_none(),
        "an expired proposal must be dropped before the vote request"
    );
    assert_eq!(
        h.epoch(),
        epoch_before,
        "a proposal the peer never saw must not commit"
    );
    assert!(h.epochs_agree(), "both members stay on the same epoch");
    assert_eq!(h.member(0).member_count(), 2, "bob is not removed");
    assert!(!h.member(1).saw_leaving(), "bob does not leave");
}
