//! PendingJoin expiration → eviction.
//!
//! A user calls `start_conversation(..., false)` (joiner intent) for a
//! conversation that has no creator on the network. After `3 ×
//! commit_inactivity_duration` the polling cycle sets `leave_requested`;
//! the caller then runs [`User::finalize_self_leave`] to drop the registry
//! entry and broadcast removal. This test asserts the full cleanup pathway
//! end-to-end.

use std::time::{Duration, Instant};

use de_mls::core::{SessionEvent, StewardListConfig};
use de_mls::session::ConversationConfig;
use de_mls_gateway::user::ConversationLifecycle;

mod common;
use common::session_fixtures::{SessionArc, make_user, settle_for};

const ALICE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[test]
fn pending_join_expires_evicts_entry_and_broadcasts_removal() {
    let group = "ghost-group";
    let inactivity = Duration::from_millis(80);
    let cfg = ConversationConfig {
        commit_inactivity_duration: inactivity,
        ..ConversationConfig::default()
    };
    let steward_cfg = StewardListConfig::new(1, 5).unwrap();

    let (mut alice, _h) = make_user(ALICE_KEY, cfg, steward_cfg);

    // Alice joins a conversation no one else is in — no welcome will ever
    // arrive. The session sits in PendingJoin.
    alice.start_conversation(group, false).unwrap();
    assert!(
        alice
            .list_conversations()
            .unwrap()
            .contains(&group.to_string())
    );

    let session = alice
        .lookup_entry(group)
        .unwrap()
        .expect("session registered");

    // Poll until expiry. The first tick after start anchors the timer; we
    // need ≥ 3× inactivity to fire the expiry.
    let leave_requested = await_leave_requested(&session, inactivity);
    assert!(
        leave_requested,
        "polling must signal leave_requested once 3× inactivity has passed"
    );

    // The session must have emitted Leaving before returning leave_requested.
    let session_events = session.read().unwrap().drain_events();
    assert!(
        session_events
            .iter()
            .any(|e| matches!(e, SessionEvent::Leaving)),
        "session must emit Leaving on PendingJoin expiry"
    );

    // Cleanup pathway: caller follows up with finalize_self_leave.
    alice.finalize_self_leave(group).unwrap();

    // Registry entry is gone.
    assert!(
        !alice
            .list_conversations()
            .unwrap()
            .contains(&group.to_string()),
        "registry entry must be evicted"
    );
    assert!(
        alice.lookup_entry(group).unwrap().is_none(),
        "lookup_entry must return None after eviction"
    );

    // User-level lifecycle drain surfaces Created + Removed.
    let lifecycle_events = alice.drain_lifecycle_events();
    assert!(
        lifecycle_events
            .iter()
            .any(|e| matches!(e, ConversationLifecycle::Removed(name) if name == group)),
        "lifecycle drain must include Removed"
    );
}

fn await_leave_requested(session: &SessionArc, inactivity: Duration) -> bool {
    // Allow up to 6× inactivity so the test isn't fragile on slow CI.
    let deadline = Instant::now() + inactivity * 6;
    loop {
        let outcome = session.write().unwrap().poll().unwrap();
        if outcome.leave_requested {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        settle_for(inactivity / 4);
    }
}
