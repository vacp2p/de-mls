//! Layer-3 recovery convergence: when the sole steward goes silent, a member
//! opens recovery through consensus (`request_recovery`) and the online members
//! auto-mint the stuck work, converging on a single commit — no fork.

mod common;

use std::time::Duration;

use common::harness::{TestHarness, fast_config};
use de_mls::{ConversationConfig, ConversationEvent, ConversationState, StewardListConfig};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

#[test]
fn recovery_auto_mint_converges_on_one_commit() {
    // Immediate auto-mint, short recovery settle: every online node mints as
    // soon as recovery opens, so the test exercises the multi-minter path.
    let cfg = ConversationConfig {
        recovery_auto_commit_delay: Some(Duration::ZERO),
        recovery_inactivity_duration: Duration::from_millis(50),
        ..fast_config()
    };
    // sn_max = 1: a single steward, so muting it leaves no online committer and
    // the approved work can only land through recovery.
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "rc",
        cfg,
        StewardListConfig::new(1, 1).unwrap(),
    );
    for _ in 0..5 {
        h.process(Duration::from_millis(40));
    }

    // The sole epoch steward goes dark; the other two stay online.
    let steward = (0..3)
        .find(|&i| h.member(i).convo().is_epoch_steward().unwrap())
        .expect("a sole epoch steward exists");
    let online: Vec<usize> = (0..3).filter(|&i| i != steward).collect();
    let (a, b) = (online[0], online[1]);
    let steward_id = h.member(steward).member_id_bytes().to_vec();
    h.mute(steward);

    // A non-steward moves to remove the silent steward — consensus among the two
    // online members approves it, but with no online steward it can't commit —
    // then opens recovery through the real Deadlock-ECP consensus path.
    h.member_mut(a).remove_member(&steward_id);
    h.member_mut(a).request_recovery();

    // Recovery auto-mints on both online members; they must converge on the
    // same commit and drop the dead steward.
    h.process_until("recovery drops the dead steward", |h| {
        h.member(a).member_count() == 2 && h.member(b).member_count() == 2
    });

    // No fork: the two online members agree on epoch and membership.
    assert_eq!(
        h.member(a).epoch(),
        h.member(b).epoch(),
        "online members converge on the same epoch"
    );
    let canonical = |i: usize| {
        let mut ids = h.member(i).convo().members().unwrap();
        ids.sort();
        ids
    };
    assert_eq!(
        canonical(a),
        canonical(b),
        "online members agree on membership"
    );
    assert!(
        h.member(a).is_working() && h.member(b).is_working(),
        "both online members return to Working after recovery"
    );
    for m in [a, b] {
        assert!(
            h.member(m)
                .events()
                .iter()
                .any(|e| matches!(e, ConversationEvent::RecoveryModeOpened)),
            "member {m} observed recovery opening"
        );
        assert!(
            h.member(m).saw_phase(ConversationState::Working),
            "member {m} recovered back to Working"
        );
    }
}
