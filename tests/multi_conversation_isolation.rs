//! Per-conversation lock isolation.
//!
//! The per-conv-consensus refactor moved per-conversation state from
//! `User`-level locks onto one `RwLock` per `SessionRunner`. Headline
//! invariant: holding the write lock on one conversation's session must
//! not block reads or writes on any other conversation, and must not
//! block the outer-registry read path either.

use std::time::Duration;

use de_mls::app::ConversationConfig;
use de_mls::core::StewardListConfig;

mod common;
use common::session_fixtures::{fast_test_config, make_user};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[tokio::test]
async fn write_lock_on_one_session_does_not_block_others() {
    let cfg: ConversationConfig = fast_test_config();
    let steward_cfg = StewardListConfig::new(1, 5).unwrap();
    let (mut alice, _tx) = make_user(ALICE, cfg, steward_cfg);

    alice.start_conversation("conv-a", true).await.unwrap();
    alice.start_conversation("conv-b", true).await.unwrap();

    let session_a = alice.lookup_entry("conv-a").await.unwrap();
    let session_b = alice.lookup_entry("conv-b").await.unwrap();

    // Hold conv-a's inner write lock for the whole test. If isolation is
    // broken, the operations on conv-b below would block here.
    let a_guard = session_a.write().await;

    // Outer registry lookups touch only `Arc<RwLock<HashMap<…>>>`'s read
    // lock, not the inner SessionRunner locks. Must complete instantly.
    tokio::time::timeout(Duration::from_millis(100), alice.lookup_entry("conv-a"))
        .await
        .expect("lookup_entry must not wait on a held inner write lock");
    tokio::time::timeout(Duration::from_millis(100), alice.lookup_entry("conv-b"))
        .await
        .expect("lookup_entry on a different conversation must be unaffected");

    // Inner write lock on conv-b is a different `RwLock` instance — must
    // acquire without contention.
    {
        let mut b = tokio::time::timeout(Duration::from_millis(100), session_b.write())
            .await
            .expect("write on conv-b must not wait on conv-a's lock");
        // Drive a session-level operation to prove the lock is actually
        // usable for real work, not just acquirable.
        tokio::time::timeout(Duration::from_millis(100), b.check_member_freeze())
            .await
            .expect("check_member_freeze on conv-b must not block on conv-a")
            .unwrap();
    }

    drop(a_guard);

    // After releasing, conv-a is also accessible.
    let mut a = tokio::time::timeout(Duration::from_millis(100), session_a.write())
        .await
        .expect("conv-a write should succeed once its own guard is released");
    a.check_member_freeze().await.unwrap();
}
