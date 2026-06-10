//! Self-leave must drop the conversation's pending auto-vote registry —
//! otherwise the next `tick_deadlines` would fire against a conversation the
//! user has already left.

use std::time::Duration;

use de_mls::core::StewardListConfig;

mod common;
use common::session_fixtures::{fast_test_config, make_user};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[test]
fn finalize_self_leave_clears_pending_auto_votes() {
    let (mut user, _transport) = make_user(
        ALICE,
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    user.start_conversation("test-conv", true).unwrap();

    let session = user
        .lookup_entry("test-conv")
        .unwrap()
        .expect("creator session registered");

    // Seed a pending auto-vote with a far-future fire-at so the assertion
    // isn't sensitive to wall-clock drift.
    session
        .write()
        .unwrap()
        .register_auto_vote(42, Duration::from_secs(600), true);
    assert!(
        session.read().unwrap().pending_auto_votes.contains_key(&42),
        "auto-vote must be registered before self-leave"
    );

    user.finalize_self_leave("test-conv").unwrap();

    // The registry entry is gone, so the conversation's pending auto-votes
    // can no longer fire from a poll cycle on this user.
    assert!(
        user.lookup_entry("test-conv").unwrap().is_none(),
        "registry entry must be evicted on self-leave"
    );
}
