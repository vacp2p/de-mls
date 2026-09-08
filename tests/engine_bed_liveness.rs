//! Liveness on the fake bed: a silent epoch steward is reported and then
//! skipped by a recovery vote, a foreign commit is discarded and scored,
//! removing a steward elects a fresh list, a candidate rejected as not
//! the epoch steward's comes back once the skip lands, and a steward
//! skipped while away follows the substitute's commit on its return.

mod common;

use std::time::Duration;

use common::fake_router::Bed;
use common::fast_config;
use de_mls::engine::{Action, CandidateRejection, Decision, Event, Verdict};

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

#[test]
fn a_substitute_commit_that_beats_the_verdict_is_staged_again_after_the_skip() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let dave = bed.seat(0, "dave");
    let erin = bed.seat(0, "erin");
    let members = [0, bob, carol, dave, erin];

    let es = bed.epoch_steward();
    let es_id = bed.nodes[es].own.clone();
    let s = members
        .into_iter()
        .find(|&i| bed.router(i).engine.is_steward() && !bed.router(i).engine.is_epoch_steward())
        .expect("a steward that is not the epoch steward exists among five members");
    let s_id = bed.nodes[s].own.clone();
    let pqr: Vec<usize> = members
        .into_iter()
        .filter(|&i| !bed.router(i).engine.is_steward())
        .collect();
    let (p, q, r) = (pqr[0], pqr[1], pqr[2]);

    bed.mute(es);

    let frank = bed.add_pending("frank");
    let kp_frank = bed.key_package_of(frank);
    let frank_id = bed.nodes[frank].own.clone();
    let now = bed.now;
    bed.router_mut(p).announce(now, frank_id, kp_frank);

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

    // q still gets p's proposal and casts its own vote, but the votes of s
    // and r reach it late, so s's commit lands at q before the verdict does.
    let hold_start = bed.now;
    let q_events_before = bed.router(q).events.len();
    let q_decisions_before = bed.router(q).decisions.len();
    bed.hold_control(q, &[s, r], hold_start + Duration::from_secs(2));

    let now = bed.now;
    bed.router_mut(p)
        .act(now, |e| e.request_recovery(now))
        .expect("request recovery");

    bed.process_until("q rejected the early candidate", |b| {
        b.router(q).rejected_commits() == 1
    });
    assert!(
        !bed.router(q)
            .events
            .iter()
            .any(|e| matches!(e, Event::StewardSkipped { .. })),
        "q has not drained the skip yet"
    );
    assert!(
        bed.router(q).events[q_events_before..]
            .iter()
            .any(|e| matches!(
                e,
                Event::CandidateRejected {
                    sender,
                    reason: CandidateRejection::NotEpochSteward,
                } if *sender == s_id
            )),
        "q rejected s's early candidate as not the epoch steward's"
    );

    bed.process_until("everyone including q seated frank", |b| {
        b.is_live(frank) && b.agree_among(&[p, q, r, s, frank])
    });

    assert!(
        bed.router(q)
            .events
            .iter()
            .any(|e| matches!(e, Event::StewardSkipped { steward, .. } if *steward == es_id))
    );

    let discard_pos = bed.router(q).decisions[q_decisions_before..]
        .iter()
        .position(|d| matches!(d, Decision::Discard { .. }))
        .expect("q discarded s's early candidate");
    assert!(
        bed.router(q).decisions[q_decisions_before + discard_pos + 1..]
            .iter()
            .any(|d| matches!(d, Decision::Merge { .. })),
        "q merged a commit after the rejection"
    );

    let s_scores: Vec<(i64, i64)> = bed.router(q).events[q_events_before..]
        .iter()
        .filter_map(|e| match e {
            Event::MemberScoreChanged {
                member,
                previous,
                score,
            } if *member == s_id => Some((*previous, *score)),
            _ => None,
        })
        .collect();
    assert!(
        !s_scores.is_empty(),
        "q scored s at least once after the rejection"
    );
    assert_eq!(
        s_scores.first().unwrap().0,
        s_scores.last().unwrap().1,
        "the rejection penalty and the merge reward for s cancel out"
    );
    assert_eq!(bed.router(q).rejected_commits(), 0);
}

// A steward that goes offline before building is skipped by a recovery
// vote it never sees. On its return it replays the vote and the
// substitute's commit and merges it through the ordinary path.
#[test]
fn a_skipped_steward_that_comes_back_follows_the_substitute() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let members = [0, bob, carol];

    let es = bed.epoch_steward();
    let es_id = bed.nodes[es].own.clone();
    let others: Vec<usize> = members.into_iter().filter(|&i| i != es).collect();
    let p = others[0];

    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(p).announce(now, dave_id, kp_dave);

    bed.process_until("all three approve dave's add", |b| {
        members.iter().all(|&i| {
            b.router(i).events.iter().any(|e| {
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

    bed.take_offline(es);

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

    let now = bed.now;
    bed.router_mut(p)
        .act(now, |e| e.request_recovery(now))
        .expect("request recovery");

    bed.process_until("dave seated among the two remaining members", |b| {
        b.is_live(dave) && b.agree_among(&others)
    });
    assert!(
        bed.router(p)
            .events
            .iter()
            .any(|e| matches!(e, Event::StewardSkipped { steward, .. } if *steward == es_id))
    );

    let es_events_mark = bed.router(es).events.len();
    let es_decisions_mark = bed.router(es).decisions.len();

    bed.come_back(es);

    // The replay runs the steward's stale round, whose build the router
    // drops; the live check starts after it.
    let es_decisions_after_replay = bed.router(es).decisions.len();

    bed.process_until("the steward converges with dave seated", |b| {
        b.converged() && b.is_live(dave)
    });

    assert_eq!(bed.router(es).mls.members().len(), 4);
    assert!(
        bed.router(es).events[es_events_mark..]
            .iter()
            .any(|e| matches!(e, Event::StewardSkipped { steward, .. } if *steward == es_id)),
        "the steward replays its own skip"
    );
    assert!(
        bed.router(es).decisions[es_decisions_mark..]
            .iter()
            .any(|d| matches!(d, Decision::Merge { .. })),
        "the steward merges the substitute's commit through the ordinary path"
    );
    assert!(
        !bed.router(es).decisions[es_decisions_after_replay..]
            .iter()
            .any(|d| matches!(d, Decision::BuildCommit { .. })),
        "the steward builds nothing once it is back on the live path"
    );
    assert!(
        !bed.router(es).events[es_events_mark..]
            .iter()
            .any(|e| matches!(e, Event::CommitAdopted { .. })),
        "the commit merges through a decision, not adoption"
    );
    assert!(
        !bed.router(es).events[es_events_mark..]
            .iter()
            .any(|e| matches!(e, Event::CandidateRejected { .. })),
        "the steward does not reject the substitute's replayed candidate"
    );
}
