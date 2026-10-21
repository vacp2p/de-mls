//! Membership changes on the fake bed: a voted removal, a self-leave, the
//! sole steward leaving, a rejected invite that never lands, and
//! steward-list exhaustion by growth.

mod common;

use common::fake_mls::FakeMls;
use common::fake_router::{Bed, Step};
use common::fast_config;
use de_mls::engine::{Action, Event, Phase, Verdict};
use de_mls::steward_list::StewardListConfig;

#[test]
fn removal_is_voted_and_the_removed_member_leaves() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let bob_id = bed.nodes[bob].own.clone();

    let now = bed.now;
    bed.router_mut(0)
        .act(now, |e, p| e.propose_remove(now, bob_id.clone(), p))
        .expect("propose remove bob");

    bed.process_until("bob left and node0/carol converge", |b| {
        b.router(bob).left && b.agree_among(&[0, carol]) && b.router(0).mls.members().len() == 2
    });

    assert!(bed.agree_among(&[0, carol]));
    assert_eq!(bed.router(0).mls.members().len(), 2);
    assert_eq!(bed.router(carol).mls.members().len(), 2);
    assert!(matches!(bed.router(bob).steps.last(), Some(Step::Leave)));
}

#[test]
fn self_leave_lands_in_the_next_commit() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");

    let now = bed.now;
    bed.router_mut(bob)
        .act(now, |e, p| e.leave(now, p))
        .expect("bob leaves");

    bed.process_until("bob left and node0/carol converge", |b| {
        b.router(bob).left && b.agree_among(&[0, carol]) && b.router(0).mls.members().len() == 2
    });

    assert!(bed.agree_among(&[0, carol]));
    assert_eq!(bed.router(0).mls.members().len(), 2);
    assert_eq!(bed.router(carol).mls.members().len(), 2);
    assert!(matches!(bed.router(bob).steps.last(), Some(Step::Leave)));
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
    // Guard: the seating returns once the joiner is live, and the proposal
    // below needs every node out of `Syncing`.
    bed.process_until("every node is synced", |b| {
        b.live_nodes()
            .iter()
            .all(|&n| b.router(n).engine.is_synced())
    });
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
        bed.router(steward).steps.last(),
        Some(Step::Leave)
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
            .act(now, |e, p| e.leave(now, p))
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
            .act(now, |e, p| e.propose_remove(now, target, p))
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
        .act(now, |e, p| e.vote(now, proposal_id, false, p))
        .expect("bob votes no");
    let now = bed.now;
    bed.router_mut(carol)
        .act(now, |e, p| e.vote(now, proposal_id, false, p))
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

// A key package expires between the vote and the build: the epoch steward's
// group refuses it, the steward drops the approved add and tells the group,
// every node drops the same add, and no epoch moves. The sponsor files the
// joiner again with a fresh key package and the second vote seats it.
#[test]
fn an_expired_key_package_is_withdrawn_and_the_joiner_is_invited_again() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    bed.seat(0, "bob");
    bed.seat(0, "carol");
    bed.process_until("every node is synced", |b| {
        b.live_nodes()
            .iter()
            .all(|&n| b.router(n).engine.is_synced())
    });
    let live = bed.live_nodes();

    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let marks: Vec<usize> = live.iter().map(|&n| bed.router(n).events.len()).collect();
    let now = bed.now;
    bed.router_mut(0)
        .announce(now, dave_id.clone(), kp_dave.clone());

    bed.process_until("every node approved dave's add", |b| {
        live.iter().zip(&marks).all(|(&n, &mark)| {
            b.router(n).events[mark..].iter().any(|e| {
                matches!(
                    e,
                    Event::ConsensusReached {
                        verdict: Verdict::Approved,
                        ..
                    }
                )
            })
        })
    });
    let steward = bed.epoch_steward();
    bed.router_mut(steward).mls.expire_key_package(&kp_dave);
    let epoch_before = bed.router(0).mls.epoch();

    let refused = Action::Add {
        member: dave_id.clone(),
        key_package: kp_dave,
    };
    bed.process_until("every node dropped dave's add", |b| {
        live.iter().zip(&marks).all(|(&n, &mark)| {
            b.router(n).events[mark..].iter().any(|e| {
                matches!(e, Event::ActionsDropped { actions } if actions == &vec![refused.clone()])
            })
        })
    });
    assert!(!bed.is_live(dave));
    assert_eq!(bed.router(0).mls.epoch(), epoch_before);
    assert!(bed.agree_among(&live));

    let now = bed.now;
    bed.router_mut(0)
        .announce(now, dave_id, FakeMls::key_package_v("dave", 2));
    bed.process_until("dave seated", |b| {
        b.is_live(dave) && b.epochs_agree() && b.router(dave).engine.is_synced()
    });

    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 4);
}

// The steward list runs out at an epoch boundary while members keep
// joining: alice enters `Syncing` for the election, it lands, and seating
// goes on without a stall until six members are seated and every live node
// is `Working`.
#[test]
fn an_exhausted_steward_list_is_replaced_without_a_stall() {
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
