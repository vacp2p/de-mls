//! Per-conversation lock isolation.
//!
//! Each `SessionRunner` lives behind its own `RwLock` inside the User's
//! conversation registry. Holding one session's write lock must not
//! block reads or writes on any other session, nor block the outer
//! registry read path that `lookup_entry` walks.

use de_mls::app::{ConversationConfig, SessionRunner};
use de_mls::core::StewardListConfig;

mod common;
use common::session_fixtures::{fast_test_config, make_user};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

// The whole point of this test is to hold one session's guard while
// driving work on another — the await-holding-lock the lint flags is the
// exact thing under assertion.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn write_lock_on_one_session_does_not_block_others() {
    let cfg: ConversationConfig = fast_test_config();
    let steward_cfg = StewardListConfig::new(1, 5).unwrap();
    let (mut alice, _tx) = make_user(ALICE, cfg, steward_cfg);

    alice.start_conversation("conv-a", true).await.unwrap();
    alice.start_conversation("conv-b", true).await.unwrap();

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
        // Guard is dropped at the end of this block, before the await
        // below — `check_member_freeze` takes the Arc and locks itself.
    }

    // Drive a session-level operation to prove the lock is actually
    // usable for real work, not just acquirable. `check_member_freeze`
    // is a static method that takes the Arc and locks internally, so it
    // composes with the outer isolation check without holding any guard
    // across an await.
    SessionRunner::check_member_freeze(&session_b)
        .await
        .expect("check_member_freeze on conv-b must succeed");

    drop(a_guard);

    // After releasing, conv-a is also accessible — and the same static
    // entry point works against either Arc.
    SessionRunner::check_member_freeze(&session_a)
        .await
        .expect("conv-a check_member_freeze must succeed");
}
