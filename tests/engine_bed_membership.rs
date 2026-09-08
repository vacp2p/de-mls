//! Membership changes on the fake bed: a voted removal, a self-leave, the
//! sole steward leaving, a rejected invite that never lands, and
//! steward-list exhaustion by growth.

mod common;

use common::fake_router::Bed;
use common::fast_config;
use de_mls::engine::{Decision, Event, Phase, Verdict};
use de_mls::steward_list::StewardListConfig;

#[test]
fn removal_is_voted_and_the_removed_member_leaves() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let bob_id = bed.nodes[bob].own.clone();

    let now = bed.now;
    bed.router_mut(0)
        .act(now, |e| e.propose_remove(now, bob_id.clone()))
        .expect("propose remove bob");

    bed.process_until("bob left and node0/carol converge", |b| {
        b.router(bob).left && b.agree_among(&[0, carol]) && b.router(0).mls.members().len() == 2
    });

    assert!(bed.agree_among(&[0, carol]));
    assert_eq!(bed.router(0).mls.members().len(), 2);
    assert_eq!(bed.router(carol).mls.members().len(), 2);
    assert!(matches!(
        bed.router(bob).decisions.last(),
        Some(Decision::Leave)
    ));
}

#[test]
fn self_leave_lands_in_the_next_commit() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");

    let now = bed.now;
    bed.router_mut(bob)
        .act(now, |e| e.leave(now))
        .expect("bob leaves");

    bed.process_until("bob left and node0/carol converge", |b| {
        b.router(bob).left && b.agree_among(&[0, carol]) && b.router(0).mls.members().len() == 2
    });

    assert!(bed.agree_among(&[0, carol]));
    assert_eq!(bed.router(0).mls.members().len(), 2);
    assert_eq!(bed.router(carol).mls.members().len(), 2);
    assert!(matches!(
        bed.router(bob).decisions.last(),
        Some(Decision::Leave)
    ));
}

// The only steward on a one-member list is on its way out. Nobody eligible
// is left to commit, so the engine elects a fresh list without the leaver
// on its own; the new steward commits the removal and every remaining node
// accepts that commit as the steward's. The router does nothing.
fn sole_steward_removal_is_committed_by_a_fresh_steward(remove: impl FnOnce(&mut Bed, usize)) {
    let mut config = fast_config();
    config.steward_list = StewardListConfig::new(1, 1).expect("one steward");
    let mut bed = Bed::new("conv", "alice", config);
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let steward = bed.epoch_steward();
    let others: Vec<usize> = [0, bob, carol]
        .into_iter()
        .filter(|&n| n != steward)
        .collect();
    let marks: Vec<usize> = others.iter().map(|&n| bed.router(n).events.len()).collect();

    remove(&mut bed, steward);

    bed.process_until("the steward left and the others converge", |b| {
        b.router(steward).left
            && b.agree_among(&others)
            && b.router(others[0]).mls.members().len() == 2
            && others.iter().all(|&n| b.router(n).engine.is_synced())
    });

    assert!(matches!(
        bed.router(steward).decisions.last(),
        Some(Decision::Leave)
    ));
    for (&n, &mark) in others.iter().zip(&marks) {
        assert!(
            !bed.router(n).events[mark..]
                .iter()
                .any(|e| matches!(e, Event::CandidateRejected { .. })),
            "the replacement steward's commit is the steward's on every node"
        );
    }
}

#[test]
fn sole_steward_self_leave_is_committed_by_a_fresh_steward() {
    sole_steward_removal_is_committed_by_a_fresh_steward(|bed, steward| {
        let now = bed.now;
        bed.router_mut(steward)
            .act(now, |e| e.leave(now))
            .expect("the steward leaves");
    });
}

#[test]
fn sole_steward_voted_removal_is_committed_by_a_fresh_steward() {
    sole_steward_removal_is_committed_by_a_fresh_steward(|bed, steward| {
        let proposer = (0..3).find(|&n| n != steward).expect("another member");
        let target = bed.nodes[steward].own.clone();
        let now = bed.now;
        bed.router_mut(proposer)
            .act(now, |e| e.propose_remove(now, target))
            .expect("propose remove the steward");
    });
}

// Two NO votes against the filer's bundled YES reject the add; the invite
// never lands and the member set is unchanged on every node.
#[test]
fn rejected_invite_never_lands() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");

    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, dave_id, kp_dave);

    let before = bed.router(bob).events.len();
    bed.process_until("bob asked to vote", |b| {
        b.router(bob).events[before..]
            .iter()
            .any(|e| matches!(e, Event::VoteRequested { .. }))
    });
    let proposal_id = bed.router(bob).events[before..]
        .iter()
        .find_map(|e| match e {
            Event::VoteRequested { proposal_id, .. } => Some(*proposal_id),
            _ => None,
        })
        .expect("bob was asked to vote");

    let now = bed.now;
    bed.router_mut(bob)
        .act(now, |e| e.vote(now, proposal_id, false))
        .expect("bob votes no");
    let now = bed.now;
    bed.router_mut(carol)
        .act(now, |e| e.vote(now, proposal_id, false))
        .expect("carol votes no");

    bed.process_until("consensus reached", |b| {
        b.router(0).events.iter().any(|e| {
            matches!(
                e,
                Event::ConsensusReached {
                    verdict: Verdict::Rejected,
                    ..
                }
            )
        })
    });

    let epoch_before = bed.router(0).mls.epoch();
    bed.process(std::time::Duration::from_secs(2));

    assert!(!bed.is_live(dave));
    assert_eq!(bed.router(0).mls.epoch(), epoch_before);
    assert!(bed.agree_among(&[0, bob, carol]));
}

// Backlog 32: at four settled members the steward list (`sn_max` == 2)
// exhausts into a genuine subset. Every node enters `Syncing`, alice files
// the election, it lands, everyone returns to `Working`, and a fifth
// member seats normally afterward.
#[test]
fn five_members_keep_committing() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    bed.seat(0, "bob");
    bed.seat(0, "carol");
    bed.seat(0, "dave");
    bed.seat(0, "eve");
    bed.seat(0, "frank");

    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 6);
    assert!(
        bed.router(0)
            .events
            .iter()
            .any(|e| matches!(e, Event::PhaseChange(Phase::Syncing))),
        "the list ran out and an election replaced it"
    );
    assert!(
        bed.live_nodes()
            .iter()
            .all(|&n| bed.router(n).engine.phase() == Phase::Working)
    );
}
