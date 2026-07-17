//! Layer-3 recovery convergence: when the sole steward goes silent, a member
//! opens recovery through consensus (`request_recovery`) and the online members
//! mint the stuck work (via the app's recovery policy), converging on a single
//! commit — no fork.

mod common;

use std::time::Duration;

use common::harness::{TestHarness, fast_config};
use de_mls::{ConversationConfig, ConversationEvent, ConversationState, StewardListConfig};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const DAVE: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

#[test]
fn recovery_auto_mint_converges_on_one_commit() {
    // Short recovery settle. The harness's default recovery policy mints on
    // every online node as soon as recovery opens, so the test exercises the
    // multi-minter path.
    let cfg = ConversationConfig {
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

    // The harness recovery policy mints on both online members; they must
    // converge on the same commit and drop the dead steward.
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

#[test]
fn manual_only_recovery_does_not_auto_commit() {
    // Manual-only policy: recovery opens but no member calls commit_in_recovery,
    // so the stuck removal must not auto-commit on its own.
    let cfg = ConversationConfig {
        recovery_inactivity_duration: Duration::from_millis(50),
        ..fast_config()
    };
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "rn",
        cfg,
        StewardListConfig::new(1, 1).unwrap(),
    );
    h.set_recovery_auto_commit(false);
    for _ in 0..5 {
        h.process(Duration::from_millis(40));
    }

    let steward = (0..3)
        .find(|&i| h.member(i).convo().is_epoch_steward().unwrap())
        .expect("a sole epoch steward exists");
    let online: Vec<usize> = (0..3).filter(|&i| i != steward).collect();
    let steward_id = h.member(steward).member_id_bytes().to_vec();
    h.mute(steward);

    h.member_mut(online[0]).remove_member(&steward_id);
    h.member_mut(online[0]).request_recovery();

    // Let recovery open and several windows pass — with the recovery policy off
    // the removal cannot land.
    for _ in 0..12 {
        h.process(Duration::from_millis(40));
    }

    assert_eq!(
        h.member(online[0]).member_count(),
        3,
        "manual-only recovery must not auto-commit the removal"
    );
    assert!(
        h.member(online[0]).convo().is_in_recovery_mode(),
        "recovery stays open, waiting for a manual commit"
    );
}

#[test]
fn sole_steward_offline_recovers_and_commits_approved_add() {
    // The filed deadlock: a 2-member group whose installed steward list holds
    // only the (offline) creator, with an approved Add stuck behind it. The
    // remaining member must claim election-proposer authority — the accused
    // steward loses it, and authority falls through to the lowest eligible
    // candidate — install a fresh list, commit the Add, and welcome the joiner.
    //
    // The long commit-inactivity against process_until's 30 s virtual budget
    // also pins the post-election window: the freeze wait before reelection
    // already costs ~20 s, so the recovered steward must commit within the
    // short recovery window — waiting out a second full commit-inactivity
    // epoch would blow the budget.
    let cfg = ConversationConfig {
        recovery_inactivity_duration: Duration::from_millis(100),
        ..fast_config()
    };
    let mut h = TestHarness::<3>::start(
        [ALICE, BOB, CHARLIE],
        "sole",
        cfg,
        StewardListConfig::new(1, 1).unwrap(),
    );
    // The app's wide commit-inactivity: the freeze wait before reelection costs
    // ~20 s, so this pins the post-election short-window behavior.
    h.set_commit_inactivity(Duration::from_secs(20));
    let conversation_id = h.conversation_id().to_string();

    // Bob joins; Charlie only announces later, so the group is Alice + Bob
    // with Alice the sole steward.
    let bob_announcement = h.member_mut(1).announce_key_package(&conversation_id);
    h.deliver_key_package_all(&bob_announcement);
    h.process_until("bob joins", |h| h.member(1).is_working());

    // Charlie's announced join reaches consensus on Bob...
    let charlie_announcement = h.member_mut(2).announce_key_package(&conversation_id);
    h.deliver_key_package_all(&charlie_announcement);
    h.process_until("charlie's add is approved on bob", |h| {
        h.member(1).approved_count() == 1
    });

    // ...and Alice goes dark before committing it.
    h.mute(0);

    // Bob must not park in Reelection: the freeze timeout accuses Alice,
    // authority falls through to Bob, a fresh list installs, and Bob commits
    // the stuck Add — Charlie receives its welcome.
    h.process_until("bob recovers and charlie joins", |h| {
        h.member(1).member_count() == 3 && h.member(2).is_working()
    });

    assert!(
        h.member(1).saw_phase(ConversationState::Reelection),
        "bob went through the reelection stall path"
    );
    assert_eq!(
        h.member(1).epoch(),
        h.member(2).epoch(),
        "bob and charlie agree on the epoch"
    );
}

#[test]
fn silent_election_proposer_hands_authority_to_next_member() {
    // Deeper stall: the sole steward is offline AND the member that authority
    // first falls through to is offline as well. The reelection-silence
    // watchdog must count the empty window as a rejected round, rotating
    // proposer authority to the next online member until the stuck removal
    // lands.
    let cfg = ConversationConfig {
        recovery_inactivity_duration: Duration::from_millis(50),
        ..fast_config()
    };
    let mut h = TestHarness::<4>::bootstrap(
        [ALICE, BOB, CHARLIE, DAVE],
        "silent",
        cfg,
        StewardListConfig::new(1, 1).unwrap(),
    );
    for _ in 0..5 {
        h.process(Duration::from_millis(40));
    }

    let steward = (0..4)
        .find(|&i| h.member(i).is_epoch_steward())
        .expect("a sole epoch steward exists");
    let steward_id = h.member(steward).member_id_bytes().to_vec();
    // With the steward's removal approved, round-0 proposer authority falls
    // to the lowest remaining member id — take that member offline too.
    let mut others: Vec<usize> = (0..4).filter(|&i| i != steward).collect();
    others.sort_by(|&a, &b| {
        h.member(a)
            .member_id_bytes()
            .cmp(h.member(b).member_id_bytes())
    });
    let (silent_proposer, online) = (others[0], [others[1], others[2]]);
    h.mute(steward);
    h.mute(silent_proposer);

    // Approve the steward's removal among the three non-steward members
    // (two online + the silent proposer, who still receives and votes but
    // whose outbound is dropped; the silent peers count as YES at timeout).
    h.member_mut(online[0]).remove_member(&steward_id);

    h.process_until("removal lands despite two silent members", |h| {
        online
            .iter()
            .all(|&m| h.member(m).member_count() == 3 && h.member(m).is_working())
    });

    let retried = online
        .iter()
        .any(|&m| h.member(m).saw_phase(ConversationState::Reelection));
    assert!(retried, "the online members went through reelection");
    assert_eq!(
        h.member(online[0]).epoch(),
        h.member(online[1]).epoch(),
        "no fork between the online members"
    );
}
