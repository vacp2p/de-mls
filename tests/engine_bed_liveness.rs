//! Liveness on the fake bed: a silent epoch steward is reported and then
//! skipped by a recovery vote, a foreign commit is discarded and scored, and
//! removing a steward elects a fresh list.

mod common;

use common::fake_router::Bed;
use common::fast_config;
use de_mls::engine::{Action, Decision, Event};

#[test]
fn silent_epoch_steward_is_reported_then_skipped_by_a_recovery_vote() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");

    let es = bed.epoch_steward();
    bed.mute(es);
    let es_id = bed.nodes[es].own.clone();
    let p = [0, bob, carol].into_iter().find(|&i| i != es).unwrap();

    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(p).announce(now, dave_id, kp_dave);

    bed.process_until("p sees a silent epoch steward", |b| {
        b.router(p).events.iter().any(|e| {
            matches!(
                e,
                Event::CommitMissing {
                    steward: Some(_),
                    ..
                }
            )
        })
    });
    assert!(!bed.is_live(dave));
    assert!(
        !bed.router(p)
            .events
            .iter()
            .any(|e| matches!(e, Event::StewardSkipped { .. })),
        "no recovery vote has run yet"
    );

    let now = bed.now;
    bed.router_mut(p)
        .act(now, |e| e.request_recovery(now))
        .expect("request recovery");

    bed.process_until("dave seated", |b| b.is_live(dave));

    assert!(
        bed.router(p)
            .events
            .iter()
            .any(|e| matches!(e, Event::StewardSkipped { steward, .. } if *steward == es_id))
    );
    let unmuted_plus_dave: Vec<usize> = [0, bob, carol, dave]
        .into_iter()
        .filter(|&i| i != es)
        .collect();
    assert!(bed.agree_among(&unmuted_plus_dave));
    assert_eq!(bed.router(p).mls.members().len(), 4);
    assert_ne!(bed.router(es).mls.epoch(), bed.router(p).mls.epoch());
}

#[test]
fn foreign_commit_is_discarded_and_scored() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");

    let es = bed.epoch_steward();
    let r = [0, bob, carol].into_iter().find(|&i| i != es).unwrap();
    let r_id = bed.nodes[r].own.clone();
    let carol_id = bed.nodes[carol].own.clone();
    let epoch_before = bed.router(0).mls.epoch();
    let before: Vec<(usize, usize)> = [0, bob, carol]
        .into_iter()
        .map(|node| (node, bed.router(node).events.len()))
        .collect();

    bed.router_mut(r)
        .broadcast_foreign_commit(&[Action::Remove {
            member: carol_id.clone(),
        }]);
    bed.process(std::time::Duration::from_secs(1));

    for node in [0, bob, carol] {
        if node == r {
            continue;
        }
        let since = before.iter().find(|&&(n, _)| n == node).unwrap().1;
        assert!(
            bed.router(node)
                .decisions
                .iter()
                .any(|d| matches!(d, Decision::Discard { .. })),
            "node {node} discarded the foreign commit"
        );
        let scored = bed.router(node).events[since..]
            .iter()
            .find_map(|e| match e {
                Event::MemberScoreChanged {
                    member,
                    previous,
                    score,
                } if *member == r_id => Some((*previous, *score)),
                _ => None,
            });
        let (previous, score) = scored
            .unwrap_or_else(|| panic!("node {node} never scored the foreign sender {r_id:?}"));
        assert!(score < previous, "node {node}'s score for the sender fell");
    }
    assert_eq!(bed.router(0).mls.epoch(), epoch_before);
    assert!(bed.router(0).mls.members().contains(&carol_id));
}

#[test]
fn removing_a_steward_elects_a_new_list() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let dave = bed.seat(0, "dave");

    let s = [0, bob, carol, dave]
        .into_iter()
        .find(|&i| bed.router(i).engine.is_steward() && !bed.router(i).engine.is_epoch_steward())
        .expect("a steward that is not the epoch steward exists among four members");
    let s_id = bed.nodes[s].own.clone();

    let now = bed.now;
    bed.router_mut(0)
        .act(now, |e| e.propose_remove(now, s_id))
        .expect("propose remove the steward");

    bed.process_until("s left", |b| b.router(s).left);
    let remaining: Vec<usize> = [0, bob, carol, dave]
        .into_iter()
        .filter(|&i| i != s)
        .collect();
    bed.process_until("remaining nodes resynced", |b| {
        remaining.iter().all(|&i| b.router(i).engine.is_synced())
    });

    let epoch_stewards: Vec<usize> = remaining
        .iter()
        .copied()
        .filter(|&i| bed.router(i).engine.is_epoch_steward())
        .collect();
    assert_eq!(
        epoch_stewards.len(),
        1,
        "exactly one remaining node is the epoch steward"
    );
    assert!(bed.agree_among(&remaining));
    assert_eq!(bed.router(remaining[0]).mls.members().len(), 3);
}
