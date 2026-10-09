//! The applied commit: a member that applies a commit names it by the hash of
//! the bytes it was handed and by its committer, so members handed the same
//! bytes name it alike, and a member that joined by welcome names none until it
//! applies its first.

mod common;

use std::time::Duration;

use common::harness::{TestHarness, fast_config};
use de_mls::protos::de_mls::messages::v1::{AppMessage, app_message};
use de_mls::{ConversationConfig, MemberId, Outbound, StewardListConfig};
use prost::Message;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const DAVE: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

/// Alice founds a group with bob and charlie in one commit and stays its only
/// steward for the epoch they arrive in, so the next commit is hers to mint.
fn founded<const N: usize>(keys: [&str; N], conversation_id: &str) -> TestHarness<N> {
    let mut h = TestHarness::<N>::start(
        keys,
        conversation_id,
        fast_config(),
        StewardListConfig::new(1, 1).unwrap(),
    );
    for i in 1..3 {
        let kp = h.member_mut(i).mint_key_package();
        h.member_mut(0).add_member(&kp);
    }
    h.process_until("founding commit lands", |h| {
        h.member(0).member_count() == 3 && h.members()[..3].iter().all(|m| m.is_working())
    });
    assert_eq!(h.member(0).epoch(), 1, "one founding commit");
    assert!(
        h.member(0).is_steward() && !h.member(1).is_steward() && !h.member(2).is_steward(),
        "alice is the only member that can mint the next commit"
    );
    h
}

/// `payload` with bytes appended to its commit, if it is a commit candidate.
fn with_trailing_bytes(payload: &[u8]) -> Option<Vec<u8>> {
    let mut message = AppMessage::decode(payload).ok()?;
    let Some(app_message::Payload::CommitCandidate(candidate)) = message.payload.as_mut() else {
        return None;
    };
    candidate.commit_message.extend_from_slice(b"trailing");
    Some(message.encode_to_vec())
}

#[test]
fn an_add_is_named_by_its_minter_and_a_receiver() {
    let mut h = TestHarness::<3>::start(
        [ALICE, BOB, CHARLIE],
        "add",
        fast_config(),
        StewardListConfig::new(1, 1).unwrap(),
    );
    assert_eq!(
        h.member(0).convo().applied_commit(),
        None,
        "no commit made the creation epoch"
    );

    let bob_kp = h.member_mut(1).mint_key_package();
    h.member_mut(0).add_member(&bob_kp);
    h.process_until("bob joins", |h| h.member(1).is_working());

    let first = h
        .member(0)
        .convo()
        .applied_commit()
        .expect("alice applied the commit that added bob");
    assert_eq!(
        MemberId::from(&first.committer),
        h.member(0).member_id(),
        "alice, alone in the group, minted it"
    );
    assert_eq!(
        h.member(1).convo().applied_commit(),
        None,
        "bob joined by welcome and never saw the commit that added him"
    );

    let charlie_kp = h.member_mut(2).mint_key_package();
    h.member_mut(1).add_member(&charlie_kp);
    h.process_until("charlie joins", |h| {
        h.member(2).is_working() && h.member(1).member_count() == 3
    });

    // The welcome is minted by whoever's commit won the round.
    let minter = (0..2)
        .find(|&i| {
            h.member(i)
                .welcome_readys()
                .last()
                .is_some_and(|(_, minted_locally)| *minted_locally)
        })
        .expect("one of the two minted charlie's welcome");
    let receiver = 1 - minter;

    let second = h
        .member(minter)
        .convo()
        .applied_commit()
        .expect("the minter applied its own commit");
    assert_ne!(second.hash, first.hash, "a new commit made the new epoch");
    assert_eq!(
        MemberId::from(&second.committer),
        h.member(minter).member_id(),
        "the committer is the member that minted the commit"
    );
    assert_eq!(
        h.member(receiver).convo().applied_commit(),
        Some(second),
        "handed the same bytes, the receiver names the same hash and committer"
    );
    assert_eq!(
        h.member(2).convo().applied_commit(),
        None,
        "charlie joined by welcome"
    );
}

#[test]
fn a_remove_only_commit_is_named_by_everyone_that_applies_it() {
    let mut h = founded([ALICE, BOB, CHARLIE], "remove");
    let alice = h.member(0).member_id();
    for joiner in 1..3 {
        assert_eq!(
            h.member(joiner).convo().applied_commit(),
            None,
            "member {joiner} joined by welcome"
        );
    }

    let charlie = h.member(2).member_id();
    h.member_mut(1).remove_member(charlie);
    h.process_until("charlie removed", |h| {
        h.members()[..2].iter().all(|m| m.member_count() == 2) && h.member(2).saw_leaving()
    });

    let removal = h
        .member(0)
        .convo()
        .applied_commit()
        .expect("alice applied her own commit");
    assert_eq!(
        MemberId::from(&removal.committer),
        alice,
        "the committer is the member that minted the commit"
    );
    assert_eq!(
        h.member(1).convo().applied_commit(),
        Some(removal.clone()),
        "bob, who joined by welcome, names the first commit he applies"
    );
    assert_eq!(
        h.member(2).convo().applied_commit(),
        Some(removal),
        "the removed member names the commit that removed it"
    );
}

#[test]
fn a_commit_that_removes_and_adds_is_named_by_everyone_that_applies_it() {
    let mut h = founded([ALICE, BOB, CHARLIE, DAVE], "swap");
    let alice = h.member(0).member_id();

    let charlie = h.member(2).member_id();
    let dave_kp = h.member_mut(3).mint_key_package();
    h.member_mut(0).remove_member(charlie);
    h.member_mut(0).add_member(&dave_kp);
    h.process_until("charlie out, dave in", |h| {
        h.member(2).saw_leaving() && h.member(3).is_working() && h.member(1).epoch() > 1
    });
    assert_eq!(h.member(0).epoch(), 2, "one commit carried both changes");

    let swap = h
        .member(0)
        .convo()
        .applied_commit()
        .expect("alice applied her own commit");
    assert_eq!(
        MemberId::from(&swap.committer),
        alice,
        "the committer is the member that minted the commit"
    );
    assert_eq!(
        h.member(1).convo().applied_commit(),
        Some(swap.clone()),
        "handed the same bytes, bob names the same hash and committer"
    );
    assert_eq!(
        h.member(2).convo().applied_commit(),
        Some(swap),
        "the removed member names the commit that removed it"
    );
    assert_eq!(
        h.member(3).convo().applied_commit(),
        None,
        "dave joined by welcome"
    );
}

/// Recovery relaxes who may commit, not how a commit is named. With the sole
/// steward silent, a member that is no steward mints the stuck removal by hand.
#[test]
fn a_recovery_commit_is_named_by_everyone_that_applies_it() {
    // Manual-only recovery, so exactly one member mints.
    let cfg = ConversationConfig {
        recovery_auto_commit_delay: None,
        retry_window: Duration::from_millis(50),
        ..fast_config()
    };
    let mut h = TestHarness::<3>::bootstrap(
        [ALICE, BOB, CHARLIE],
        "recover",
        cfg,
        StewardListConfig::new(1, 1).unwrap(),
    );
    for _ in 0..5 {
        h.process(Duration::from_millis(40));
    }

    let steward = (0..3)
        .find(|&i| h.member(i).convo().is_epoch_steward().unwrap())
        .expect("a sole epoch steward exists");
    let online: Vec<usize> = (0..3).filter(|&i| i != steward).collect();
    let (minter, receiver) = (online[0], online[1]);
    let minter_id = h.member(minter).member_id();
    let steward_id = h.member(steward).member_id();
    h.mute(steward);

    h.member_mut(minter).remove_member(steward_id);
    h.member_mut(minter).request_recovery();
    h.process_until("recovery opens with the removal approved", |h| {
        online.iter().all(|&m| {
            h.member(m).convo().is_in_recovery_mode() && h.member(m).approved_count() == 1
        })
    });
    assert!(
        h.member_mut(minter).commit_in_recovery(),
        "the minter holds the approved removal"
    );
    // The silent steward still hears the commit that removes it.
    h.process_until("the recovery commit lands", |h| {
        online.iter().all(|&m| h.member(m).member_count() == 2) && h.member(steward).saw_leaving()
    });

    let recovery = h
        .member(minter)
        .convo()
        .applied_commit()
        .expect("the minter applied its own commit");
    assert_eq!(
        MemberId::from(&recovery.committer),
        minter_id,
        "the committer is the member that minted the commit"
    );
    assert_eq!(
        h.member(receiver).convo().applied_commit(),
        Some(recovery.clone()),
        "handed the same bytes, the receiver names the same hash and committer"
    );
    assert_eq!(
        h.member(steward).convo().applied_commit(),
        Some(recovery),
        "the removed member names the commit that removed it"
    );
}

/// A removal for a low score commits by the urgent route, which leaves the
/// rest of the approved queue for the next cycle.
#[test]
fn an_urgent_removal_is_named_by_everyone_that_applies_it() {
    let mut h = founded([ALICE, BOB, CHARLIE], "urgent");
    let founding = h.member(0).convo().applied_commit();

    let charlie = h.member(2).member_id_bytes().to_vec();
    h.member_mut(1).propose_score_removal(&charlie);
    h.process_until("charlie removed", |h| {
        h.members()[..2].iter().all(|m| m.member_count() == 2) && h.member(2).saw_leaving()
    });

    let removal = h
        .member(0)
        .convo()
        .applied_commit()
        .expect("alice applied her own commit");
    assert_ne!(
        Some(removal.clone()),
        founding,
        "a new commit made the new epoch"
    );
    assert_eq!(
        h.member(1).convo().applied_commit(),
        Some(removal.clone()),
        "handed the same bytes, bob names the same hash and committer"
    );
    assert_eq!(
        h.member(2).convo().applied_commit(),
        Some(removal),
        "the removed member names the commit that removed it"
    );
}

/// The committer is the member that minted the commit, not the steward the
/// epoch expected one from: with the epoch steward silent, its backup commits.
#[test]
fn a_backup_that_commits_for_a_silent_epoch_steward_is_named_as_the_committer() {
    // sn_max = 5 makes every settled member a steward. Joining one commit
    // apart, alice and bob are the two stewards of the epoch charlie arrives in.
    let mut h = TestHarness::<4>::start(
        [ALICE, BOB, CHARLIE, DAVE],
        "backup",
        fast_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );
    for joiner in 1..3 {
        let kp = h.member_mut(joiner).mint_key_package();
        h.member_mut(0).add_member(&kp);
        h.process_until("the joiner lands", |h| {
            h.members()[..=joiner]
                .iter()
                .all(|m| m.is_working() && m.member_count() == joiner + 1)
        });
    }

    let silent = (0..2)
        .find(|&i| h.member(i).is_epoch_steward())
        .expect("alice or bob stewards the epoch");
    let backup = 1 - silent;
    assert!(
        h.member(backup).is_steward() && !h.member(2).is_steward(),
        "the backup is the only other member that can mint"
    );
    let silent_id = h.member(silent).member_id();
    let backup_id = h.member(backup).member_id();
    h.mute(silent);

    let dave_kp = h.member_mut(3).mint_key_package();
    h.member_mut(backup).add_member(&dave_kp);
    h.process_until("dave joins without the epoch steward", |h| {
        h.member(3).is_working() && h.member(2).member_count() == 4
    });
    assert!(
        h.member(backup)
            .welcome_readys()
            .last()
            .is_some_and(|(_, minted_locally)| *minted_locally),
        "the backup minted the commit that added dave"
    );

    for online in [backup, 2] {
        let applied = h
            .member(online)
            .convo()
            .applied_commit()
            .expect("an online member applied the backup's commit");
        let committer = MemberId::from(&applied.committer);
        assert_eq!(
            committer, backup_id,
            "member {online} names the member that minted the commit"
        );
        assert_ne!(
            committer, silent_id,
            "member {online} does not name the steward the epoch expected"
        );
    }
}

/// The hash names the bytes a member was handed, not the MLS commit in them:
/// staging reads one MLS message off the front and accepts what trails it.
#[test]
fn a_copy_with_trailing_bytes_is_the_same_commit_under_another_hash() {
    let mut h = founded([ALICE, BOB, CHARLIE, DAVE], "padded");
    let dave_kp = h.member_mut(3).mint_key_package();
    h.member_mut(0).add_member(&dave_kp);

    // Relayed by hand among the three members: alice's candidate reaches bob
    // as she sent it and charlie with bytes appended.
    let mut rounds = 0;
    while (0..3).any(|i| h.member(i).epoch() < 2) {
        rounds += 1;
        assert!(rounds <= 100, "the commit that adds dave never landed");
        for i in 0..3 {
            h.member(i).advance_clock(Duration::from_millis(50));
            h.member_mut(i).poll();
        }
        let bus: Vec<Outbound> = (0..3)
            .flat_map(|i| h.member_mut(i).take_outbound())
            .collect();
        for packet in &bus {
            let padded = with_trailing_bytes(&packet.payload);
            for i in 0..3 {
                let payload = match &padded {
                    Some(copy) if i == 2 => copy,
                    _ => &packet.payload,
                };
                h.member_mut(i).deliver_raw(&packet.sender, payload);
            }
        }
    }

    let alice = h
        .member(0)
        .convo()
        .applied_commit()
        .expect("alice applied her own commit");
    let charlie = h
        .member(2)
        .convo()
        .applied_commit()
        .expect("charlie applied the padded copy");
    assert_eq!(
        h.member(1).convo().applied_commit(),
        Some(alice.clone()),
        "handed the same bytes, bob names the same hash and committer"
    );
    assert_eq!(
        h.member(2).epoch_authenticator(),
        h.member(0).epoch_authenticator(),
        "the padded copy is the same MLS commit: charlie holds alice's state"
    );
    assert_eq!(
        charlie.committer, alice.committer,
        "charlie names the same committer"
    );
    assert_ne!(
        charlie.hash, alice.hash,
        "handed other bytes, charlie names another hash"
    );
}
