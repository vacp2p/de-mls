//! Leaving a conversation evicts the registry entry, preventing any
//! pending timers from firing on a session the member has already exited.

use std::time::Duration;

use de_mls::core::StewardListConfig;

mod common;
use common::conversation_fixtures::{fast_test_config, make_user, settle_for};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[test]
fn leave_conversation_evicts_registry_entry() {
    let (mut alice, alice_transport) = make_user(
        ALICE,
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    // Alice starts as a joiner (PendingJoin) — no welcome will arrive.
    alice.start_conversation("ghost-conv", false).unwrap();
    assert!(
        alice
            .list_conversations()
            .unwrap()
            .contains(&"ghost-conv".to_string()),
        "conversation must be registered before leave"
    );

    alice.leave_conversation("ghost-conv").unwrap();

    assert!(
        !alice
            .list_conversations()
            .unwrap()
            .contains(&"ghost-conv".to_string()),
        "conversation must be evicted after leave"
    );
    assert!(
        alice.lookup_entry("ghost-conv").unwrap().is_none(),
        "lookup must return None after eviction"
    );

    // Wait longer than voting_delay to verify no auto-vote fires post-leave.
    settle_for(Duration::from_millis(120));
    assert!(
        alice_transport.lock().unwrap().drain_packets().is_empty(),
        "no packets must be emitted after leave"
    );
}
