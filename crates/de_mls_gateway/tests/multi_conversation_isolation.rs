//! Per-conversation lock isolation.
//!
//! Each `SessionRunner` lives behind its own `RwLock` inside the User's
//! conversation registry. Holding one session's write lock must not
//! block reads or writes on any other session, nor block the outer
//! registry read path that `lookup_entry` walks.

use de_mls::core::StewardListConfig;
use de_mls::session::ConversationConfig;

mod common;
use common::session_fixtures::{fast_test_config, make_user};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

// The whole point of this test is to hold one session's guard while
// driving work on another — per-conversation lock isolation is the exact
// thing under assertion.
#[test]
fn write_lock_on_one_session_does_not_block_others() {
    let cfg: ConversationConfig = fast_test_config();
    let steward_cfg = StewardListConfig::new(1, 5).unwrap();
    let (mut alice, _tx) = make_user(ALICE, cfg, steward_cfg);

    alice.start_conversation("conv-a", true).unwrap();
    alice.start_conversation("conv-b", true).unwrap();

    let session_a = alice.lookup_entry("conv-a").unwrap().unwrap();
    let session_b = alice.lookup_entry("conv-b").unwrap().unwrap();

    // Hold conv-a's inner write lock for the whole test. If isolation is
    // broken, the operations on conv-b below would block here.
    let a_guard = session_a.write().unwrap();

    // Outer registry lookups touch only the sync `RwLock<HashMap<…>>`,
    // not the inner SessionRunner locks. The call is synchronous — it
    // returns immediately even with the inner write lock held.
    alice
        .lookup_entry("conv-a")
        .expect("lookup_entry must not wait on a held inner write lock");
    alice
        .lookup_entry("conv-b")
        .expect("lookup_entry on a different conversation must be unaffected");

    // Inner write lock on conv-b is a different `RwLock` instance — must
    // acquire without contention. `try_write` returns Ok immediately when
    // nothing's holding it; if isolation were broken, conv-a's guard
    // would force this to WouldBlock.
    {
        let _b_guard = session_b
            .try_write()
            .expect("write on conv-b must not wait on conv-a's lock");
        // Guard is dropped at the end of this block, before the
        // `poll` call below acquires its own write guard.
    }

    // Drive a session-level operation to prove the lock is actually
    // usable for real work, not just acquirable. `poll` takes `&mut self`,
    // so the caller holds the conv-b write guard for the call — independent
    // of conv-a's still-held guard.
    session_b.write().unwrap().poll();

    drop(a_guard);

    // After releasing, conv-a is also accessible — the same entry point
    // works against either session.
    session_a.write().unwrap().poll();
}
