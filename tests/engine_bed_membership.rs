//! Membership changes on the fake bed: a voted removal, a self-leave, a
//! rejected invite that never lands, and steward-list exhaustion by growth.

mod common;

use common::fake_router::Bed;
use common::fast_config;
use de_mls::engine::{Decision, Event, Phase, Verdict};

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
