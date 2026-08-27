//! Growing a group one member at a time past the first *voted* steward
//! election must not fork it. A joiner welcomed in the election epoch misses
//! the vote (no consensus session for it yet), so it must still learn the
//! elected steward list; a joiner left on the pre-election list rejects the
//! next steward's commit as unauthorized — a permanent MLS fork that equal
//! member counts hide and only the epoch authenticator reveals. Regression
//! cover for the #199 fork.

mod common;

use common::harness::{TestHarness, fast_config};
use de_mls::StewardListConfig;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const DAVE: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";
const ERIN: &str = "47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a";
const FRANK: &str = "8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba";
const GRACE: &str = "0628631ed9050dfbe4e811d3bd4fd90396c8314ad458c4801071e97cb44ae809";
const HEIDI: &str = "cef64c066877b4e37a487c2cb70735543104f53bea556e60f6eb0f4956a125a9";
const IVAN: &str = "c46755cb0f6afda5c654974581054fbd82c4a572a6972c6614ce55be1d9c22f6";
const JUDY: &str = "91d8febe864c6b456da36ef0e87fcd23bb4b6a4e0d0135187053b1a4993062e6";
const KEN: &str = "a438374c68d626ae855c1f4fe3997bc144e55d754e405f4c663e9865e3734d93";

/// `sn_max = 2` forces a voted election once the settled set exceeds two, and
/// one-at-a-time adds cross it around a group of five — the regime where a
/// joiner is welcomed in the same epoch the election runs.
#[test]
fn one_at_a_time_growth_past_election_keeps_group_converged() {
    const CONV: &str = "scale";
    let mut config = fast_config();
    config.freeze_duration = std::time::Duration::from_millis(500);
    let mut h = TestHarness::<11>::start(
        [
            ALICE, BOB, CHARLIE, DAVE, ERIN, FRANK, GRACE, HEIDI, IVAN, JUDY, KEN,
        ],
        CONV,
        config,
        StewardListConfig::new(1, 2).unwrap(),
    );

    const STEP: std::time::Duration = std::time::Duration::from_millis(50);
    for k in 1..11 {
        let kp = h.member_mut(k).mint_key_package();
        h.member_mut(0).add_member(&kp);
        // Land the add for the members that accept it (member 0 is the creator,
        // never the stranded joiner, so it always reaches the new count)...
        h.process_until(&format!("member {k} join lands"), |h| {
            h.member(k).is_working() && h.member(0).member_count() == k + 1
        });
        // ...then let any steward election this add triggered resolve and its
        // list reach every member before the next add.
        for _ in 0..30 {
            h.process(STEP);
        }
        assert!(
            h.converged(),
            "fork after adding member {k}: epoch_authenticator diverged (n={})",
            h.member(0).member_count(),
        );
    }

    assert_eq!(h.member(0).member_count(), 11, "all 11 members present");
    assert!(
        h.membership_agrees(),
        "every member sees the same member set"
    );
}
