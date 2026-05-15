//! Self-leave is idempotent on rapid back-to-back calls.
//!
//! `SessionRunner::initiate_self_leave` checks `is_pending_self_leave`
//! locally and falls back to consensus-library `ProposalAlreadyExist`
//! dedup if the proposal is still mid-vote. Either path means a second
//! `leave_conversation` call emits no new outbound packets.

use de_mls::app::ConversationConfig;
use de_mls::core::StewardListConfig;

mod common;
use common::session_fixtures::make_user;

const ALICE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[tokio::test]
async fn double_leave_does_not_re_propose() {
    let cfg = ConversationConfig::default();
    let steward_cfg = StewardListConfig::new(1, 5).unwrap();
    let (mut alice, h) = make_user(ALICE_KEY, cfg, steward_cfg);

    alice.start_conversation("c4", true).await.unwrap();
    h.drain_packets();

    alice.leave_conversation("c4").await.unwrap();
    let after_first = h.snapshot().len();
    assert!(
        after_first > 0,
        "first leave must file a self-leave proposal"
    );

    alice.leave_conversation("c4").await.unwrap();
    let after_second = h.snapshot().len();

    assert_eq!(
        after_first, after_second,
        "second leave must short-circuit and not emit a new packet"
    );
}
