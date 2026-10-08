//! The engine end to end on the fake bed: a proposal is voted, the epoch
//! steward builds, the round closes, the joiner is welcomed and synced,
//! and a second add driven by a non-steward converges the whole group.

mod common;

use std::time::Duration;

use common::fake_router::Bed;
use de_mls::EngineConfig;
use de_mls::engine::{Decision, Event, Phase, Verdict};

fn fast() -> EngineConfig {
    EngineConfig {
        commit_batch_window: Duration::from_millis(200),
        freeze_duration: Duration::from_millis(150),
        voting_delay: Duration::from_millis(100),
        consensus_timeout: Duration::from_millis(500),
        proposal_expiration: Duration::from_secs(5),
        backup_takeover_window: Duration::from_millis(300),
        ..EngineConfig::default()
    }
}

#[test]
fn creator_adds_one_then_a_member_adds_another() {
    let mut bed = Bed::new("conv", "alice", fast());
    let bob = bed.add_pending("bob");
    let kp_bob = bed.key_package_of(bob);
    let bob_id = bed.nodes[bob].own.clone();

    let now = bed.now;
    bed.router_mut(0)
        .act(now, |e| e.propose_add(now, bob_id.clone(), kp_bob))
        .expect("propose bob");

    bed.process_until("bob seated", |b| b.is_live(bob) && b.epochs_agree());
    assert_eq!(bed.router(0).mls.epoch(), 1);
    assert!(bed.converged() && bed.membership_agrees());
    assert!(
        bed.router(0)
            .events
            .iter()
            .any(|e| matches!(e, Event::MembersChanged { added, .. } if !added.is_empty())),
        "the creator reports the membership change"
    );
    bed.process_until("bob synced", |b| b.router(bob).engine.is_synced());

    // A non-steward proposes; the steward auto-votes and builds.
    let carol = bed.add_pending("carol");
    let kp_carol = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(bob)
        .act(now, |e| e.propose_add(now, carol_id.clone(), kp_carol))
        .expect("propose carol");

    bed.process_until("carol seated", |b| b.is_live(carol) && b.epochs_agree());
    assert_eq!(bed.router(0).mls.epoch(), 2);
    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(carol).mls.members().len(), 3);
    assert!(
        bed.router(bob)
            .decisions
            .iter()
            .any(|d| matches!(d, de_mls::engine::Decision::Merge { .. })),
        "bob merged the steward's commit"
    );
    assert!(
        !bed.router(bob)
            .decisions
            .iter()
            .any(|d| matches!(d, de_mls::engine::Decision::BuildCommit { .. })),
        "a non-steward never builds"
    );
}

/// A proposal filed on the epoch steward the instant its round opens is
/// proposed at once, never held. Its approval lands inside the round, after
/// the steward's build: the open round merges the member it carried on every
/// node, and the later one is seated one epoch after, with no rejected
/// candidate and no penalty for the steward.
#[test]
fn a_proposal_filed_during_a_round_lands_in_the_next_commit() {
    let mut bed = Bed::new("conv", "alice", fast());
    bed.seat(0, "bob");
    let steward = bed.epoch_steward();
    let other = 1 - steward;

    // Open a round: the non-steward announces carol.
    let carol = bed.add_pending("carol");
    let kp_carol = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(other).announce(now, carol_id, kp_carol);
    let epoch_before = bed.router(steward).mls.epoch();

    // The instant the steward reports Freezing, it announces dave.
    let seen = bed.router(steward).events.len();
    for _ in 0..3000 {
        if bed.router(steward).events[seen..]
            .iter()
            .any(|e| matches!(e, Event::PhaseChange(Phase::Freezing)))
        {
            break;
        }
        bed.process(Duration::from_millis(10));
    }
    assert_eq!(bed.router(steward).engine.phase(), Phase::Freezing);
    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    let filed_at = bed.router(steward).events.len();
    bed.router_mut(steward)
        .announce(now, dave_id.clone(), kp_dave);
    assert_eq!(
        bed.router(steward).held_announcements_count(),
        0,
        "propose_add was accepted, not held"
    );

    // Finer steps than `process_until`: the approval trails the close by
    // tens of milliseconds, inside one coarse step.
    for _ in 0..3000 {
        if bed.is_live(carol) && bed.epochs_agree() {
            break;
        }
        bed.process(Duration::from_millis(10));
    }
    assert!(bed.is_live(carol) && bed.epochs_agree(), "carol seated");
    assert_eq!(bed.router(0).mls.epoch(), epoch_before + 1);
    assert!(!bed.router(0).mls.members().contains(&dave_id));

    // Dave's approval reached the steward inside the round, before it closed.
    let events = &bed.router(steward).events[filed_at..];
    let approved = events
        .iter()
        .position(|e| {
            matches!(
                e,
                Event::ConsensusReached {
                    verdict: Verdict::Approved,
                    ..
                }
            )
        })
        .expect("dave's approval reached the steward");
    let selection = events
        .iter()
        .position(|e| matches!(e, Event::PhaseChange(Phase::Selection)))
        .expect("the round reached Selection");
    assert!(
        approved < selection,
        "dave's approval landed before the round closed"
    );

    bed.process_until("dave seated", |b| b.is_live(dave) && b.epochs_agree());
    assert_eq!(bed.router(0).mls.epoch(), epoch_before + 2);
    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 4);

    let steward_id = bed.nodes[steward].own.clone();
    for n in bed.live_nodes() {
        let events = &bed.router(n).events;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::CandidateRejected { .. })),
            "node {n} rejected a candidate"
        );
        assert!(
            events.iter().all(|e| !matches!(
                e,
                Event::MemberScoreChanged { member, previous, score }
                    if *member == steward_id && score < previous
            )),
            "node {n} lowered the steward's score"
        );
    }
}

/// A session straddling a seating commit. Carol is seated while dave's vote is
/// still open: the builder of her commit is cut off from the other member's
/// control traffic, so it has not heard dave's proposal and cannot vote. The
/// other member files dave because it is the epoch steward of the epoch carol
/// is seated into, and only the answerer's own view of its sessions is carried by a
/// sync. Carol asks for her sync, gets dave's voting session in it, and only
/// watches: she is asked for no vote and sees the verdict. The hold ends, the
/// next commit carries dave, and nobody rejects a candidate.
#[test]
fn a_session_open_at_a_seating_commit_reaches_the_new_member() {
    let mut bed = Bed::new("conv", "alice", fast());
    bed.seat(0, "bob");
    let steward = bed.epoch_steward();
    let other = 1 - steward;

    let carol = bed.add_pending("carol");
    let kp_carol = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(other).announce(now, carol_id, kp_carol);
    for _ in 0..3000 {
        if bed.router(steward).engine.phase() == Phase::Freezing {
            break;
        }
        bed.process(Duration::from_millis(10));
    }
    assert_eq!(bed.router(steward).engine.phase(), Phase::Freezing);

    // Cut the builder off from `other`, then `other` files dave.
    let now = bed.now;
    bed.hold_control(steward, &[other], now + Duration::from_millis(200));
    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(other)
        .announce(now, dave_id.clone(), kp_dave);

    for _ in 0..3000 {
        if bed.is_live(dave) && bed.epochs_agree() {
            break;
        }
        bed.process(Duration::from_millis(10));
    }
    assert!(bed.is_live(dave) && bed.epochs_agree(), "dave seated");
    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 4);

    // The builder's last vote request is dave's, after carol's own.
    let dave_proposal = bed
        .router(steward)
        .events
        .iter()
        .rev()
        .find_map(|e| match e {
            Event::VoteRequested { proposal_id, .. } => Some(*proposal_id),
            _ => None,
        })
        .expect("the builder was asked to vote on dave");
    let carol_events = &bed.router(carol).events;
    assert!(
        !carol_events
            .iter()
            .any(|e| matches!(e, Event::VoteRequested { .. })),
        "carol only watches the session she was handed"
    );
    assert!(
        carol_events.iter().any(|e| matches!(
            e,
            Event::ConsensusReached { proposal_id, verdict: Verdict::Approved, .. }
                if *proposal_id == dave_proposal
        )),
        "carol saw dave approved"
    );
    for n in bed.live_nodes() {
        assert!(
            !bed.router(n)
                .events
                .iter()
                .any(|e| matches!(e, Event::CandidateRejected { .. })),
            "node {n} rejected a candidate"
        );
    }
}

/// Carol is seated and asks for her sync; the epoch steward is cut off from
/// her request while a member files dave at carol's epoch, so the answer
/// carries dave's voting session. Carol is cut off from the proposer until
/// later, so the answer reaches her first and the proposer's own frame lands
/// on a session she already holds. She was seated at the epoch dave was filed
/// at, so she is one of his voters: she is asked to check his key package,
/// and the late frame changes nothing.
#[test]
fn a_proposal_filed_after_a_seating_reaches_the_new_member_in_any_order() {
    let config = EngineConfig {
        voting_delay: Duration::from_millis(300),
        consensus_timeout: Duration::from_millis(900),
        ..fast()
    };
    let mut bed = Bed::new("conv", "alice", config);
    bed.seat(0, "bob");

    let carol = bed.add_pending("carol");
    let kp_carol = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, carol_id, kp_carol);
    for _ in 0..3000 {
        if bed.is_live(carol) {
            break;
        }
        bed.process(Duration::from_millis(10));
    }
    assert!(bed.is_live(carol), "carol seated");
    assert!(!bed.router(carol).engine.is_synced());

    // The steward hears carol's request after dave's proposal; carol hears
    // the proposer's own frame after the steward's answer.
    let steward = bed.epoch_steward();
    let proposer = 1 - steward;
    let now = bed.now;
    let answer_at = now + Duration::from_millis(50);
    let proposer_frame_at = now + Duration::from_millis(200);
    bed.hold_control(steward, &[carol], answer_at);
    bed.hold_control(carol, &[proposer], proposer_frame_at);

    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    bed.router_mut(proposer)
        .announce(now, dave_id.clone(), kp_dave);

    let checks_dave = |b: &Bed| {
        b.router(carol).decisions.iter().any(|d| {
            matches!(
                d,
                Decision::ValidateKeyPackage { member, .. } if *member == dave_id
            )
        })
    };
    while bed.now + Duration::from_millis(10) < proposer_frame_at {
        bed.process(Duration::from_millis(10));
    }
    assert!(
        checks_dave(&bed),
        "the answer reached carol before the proposer's frame did"
    );

    bed.process_until("dave seated", |b| b.is_live(dave) && b.epochs_agree());
    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 4);
    let dave_checks = bed
        .router(carol)
        .decisions
        .iter()
        .filter(|d| matches!(d, Decision::ValidateKeyPackage { member, .. } if *member == dave_id))
        .count();
    assert_eq!(dave_checks, 1, "the late duplicate frame was a no-op");
    for n in bed.live_nodes() {
        assert!(
            !bed.router(n)
                .events
                .iter()
                .any(|e| matches!(e, Event::Error { .. })),
            "node {n} reported an error"
        );
    }
}

/// Three members; a peer files `add dave`, and once the steward is
/// `Freezing` a node still `Working` (it has heard no control traffic) files `add erin`. Both are seated, every
/// node converges, and nobody rejects a candidate or misses a commit.
#[test]
fn a_proposal_filed_by_a_working_node_during_the_freeze_converges() {
    let mut bed = Bed::new("conv", "alice", fast());
    bed.seat(0, "bob");
    bed.seat(0, "carol");
    bed.net.set_delay(Duration::from_millis(100));

    // `late` hears no control traffic for a while, so it files its own proposal
    // before the round reaches it; the other two carry `add dave` between them.
    let steward = bed.epoch_steward();
    let proposer = (0..3).find(|&n| n != steward).unwrap();
    let late = (0..3).find(|&n| n != steward && n != proposer).unwrap();
    let now = bed.now;
    // The hold must outlast the steward's freeze and the late node's proposal,
    // and end before the late node's own round closes.
    bed.hold_control(late, &[steward, proposer], now + Duration::from_millis(600));

    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(proposer)
        .announce(now, dave_id.clone(), kp_dave);
    for _ in 0..3000 {
        if bed.router(steward).engine.phase() == Phase::Freezing {
            break;
        }
        bed.process(Duration::from_millis(10));
    }
    assert_eq!(bed.router(steward).engine.phase(), Phase::Freezing);
    assert_eq!(bed.router(late).engine.phase(), Phase::Working);
    let erin = bed.add_pending("erin");
    let kp_erin = bed.key_package_of(erin);
    let erin_id = bed.nodes[erin].own.clone();
    let now = bed.now;
    bed.router_mut(late).announce(now, erin_id.clone(), kp_erin);

    bed.process_until("dave and erin seated", |b| {
        b.is_live(dave) && b.is_live(erin) && b.epochs_agree()
    });
    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 5);
    for n in bed.live_nodes() {
        assert!(
            !bed.router(n).events.iter().any(|e| matches!(
                e,
                Event::CandidateRejected { .. } | Event::CommitMissing { .. }
            )),
            "node {n} rejected a candidate or missed a commit"
        );
    }
}

/// A restart mid-vote resumes cleanly: the restarted node's consensus
/// session and its side of the vote are rebuilt from the store rather than
/// lost, and the round still converges.
#[test]
fn restart_mid_vote_resumes_and_merges() {
    let mut bed = Bed::new("conv", "alice", fast());
    let bob = bed.add_pending("bob");
    let kp_bob = bed.key_package_of(bob);
    let bob_id = bed.nodes[bob].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, bob_id.clone(), kp_bob);
    bed.process_until("bob seated", |b| b.is_live(bob) && b.epochs_agree());
    bed.process_until("bob synced", |b| b.router(bob).engine.is_synced());

    let carol = bed.add_pending("carol");
    let kp_carol = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, carol_id.clone(), kp_carol);
    bed.process_until("carol seated", |b| b.is_live(carol) && b.epochs_agree());
    bed.process_until("carol synced", |b| b.router(carol).engine.is_synced());

    // Announce a fourth member; node 1 (bob) is asked to vote on it.
    let before = bed.router(bob).events.len();
    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, dave_id.clone(), kp_dave);
    bed.process_until("bob asked to vote", |b| {
        b.router(bob).events[before..]
            .iter()
            .any(|e| matches!(e, Event::VoteRequested { .. }))
    });
    let dave_proposal = bed.router(bob).events[before..]
        .iter()
        .find_map(|e| match e {
            Event::VoteRequested { proposal_id, .. } => Some(*proposal_id),
            _ => None,
        })
        .expect("bob was asked to vote");

    // Restart bob mid-vote: its consensus session and vote are rebuilt from
    // the store rather than lost.
    bed.restart(bob);

    bed.process_until("dave seated", |b| b.is_live(dave) && b.epochs_agree());
    assert!(bed.converged() && bed.membership_agrees());
    for node in [0, bob, carol, dave] {
        assert_eq!(bed.router(node).mls.members().len(), 4);
    }
    assert!(
        bed.router(bob).events.iter().any(|e| matches!(
            e,
            Event::ConsensusReached { proposal_id, .. } if *proposal_id == dave_proposal
        )),
        "the restarted bob's session reached its verdict"
    );
}

/// A restart with a store from a past epoch is stale: the restarted node
/// drops its steward list rather than trusting an out-of-date snapshot,
/// asks for a sync, and resyncs to the group's current state.
#[test]
fn restart_with_a_stale_snapshot_resyncs() {
    let mut bed = Bed::new("conv", "alice", fast());
    let bob = bed.add_pending("bob");
    let kp_bob = bed.key_package_of(bob);
    let bob_id = bed.nodes[bob].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, bob_id.clone(), kp_bob);
    bed.process_until("bob seated", |b| b.is_live(bob) && b.epochs_agree());
    bed.process_until("bob synced", |b| b.router(bob).engine.is_synced());

    let carol = bed.add_pending("carol");
    let kp_carol = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, carol_id.clone(), kp_carol);
    bed.process_until("carol seated", |b| b.is_live(carol) && b.epochs_agree());
    bed.process_until("bob synced after carol", |b| {
        b.router(bob).engine.is_synced()
    });

    // A store from before dave joins.
    let old = bed.store_of(bob);

    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, dave_id.clone(), kp_dave);
    bed.process_until("dave seated", |b| b.is_live(dave) && b.epochs_agree());
    assert!(bed.converged() && bed.membership_agrees());

    // bob restarts from the stale, pre-dave snapshot: it drops the list it
    // held and re-syncs to the group's current membership.
    bed.restart_with(bob, old);
    assert!(!bed.router(bob).engine.is_synced());

    bed.process_until("bob resynced", |b| {
        b.router(bob).engine.is_synced() && b.converged() && b.membership_agrees()
    });
    assert_eq!(bed.router(bob).mls.members().len(), 4);
}

/// Two members file `add dave` in the same step, so two sessions run for him.
/// The first approval seats him; the second reaches the steward only after
/// that merge. It is dropped as stale when it lands, so no round opens for it: nobody
/// reports a missing commit and the steward's score never drops.
#[test]
fn a_second_approval_for_a_seated_member_is_stale() {
    let mut bed = Bed::new("conv", "alice", fast());
    bed.seat(0, "bob");
    bed.seat(0, "carol");

    let steward = bed.epoch_steward();
    let first = (0..3).find(|&n| n != steward).unwrap();
    let second = (0..3).find(|&n| n != steward && n != first).unwrap();
    let now = bed.now;
    bed.hold_control(steward, &[second], now + Duration::from_millis(1500));

    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(first)
        .announce(now, dave_id.clone(), kp_dave.clone());
    bed.router_mut(second).announce(now, dave_id, kp_dave);

    bed.process_until("dave seated", |b| b.is_live(dave) && b.epochs_agree());
    let epoch_seated = bed.router(0).mls.epoch();
    let seated_at = bed.router(steward).events.len();
    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 4);

    // The held session lands, then four quiet seconds pass.
    for _ in 0..400 {
        bed.process(Duration::from_millis(10));
    }
    assert_eq!(bed.router(0).mls.epoch(), epoch_seated, "no second commit");
    assert!(
        bed.router(steward).events[seated_at..]
            .iter()
            .any(|e| matches!(
                e,
                Event::ConsensusReached {
                    verdict: Verdict::Approved,
                    ..
                }
            )),
        "the held second session reached the steward after dave was seated"
    );
    assert!(bed.converged() && bed.membership_agrees());
    let steward_id = bed.nodes[steward].own.clone();
    for n in bed.live_nodes() {
        let events = &bed.router(n).events;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::CommitMissing { .. })),
            "node {n} reported a missing commit"
        );
        assert!(
            events.iter().all(|e| !matches!(
                e,
                Event::MemberScoreChanged { member, previous, score }
                    if *member == steward_id && score < previous
            )),
            "node {n} lowered the steward's score"
        );
    }
}
