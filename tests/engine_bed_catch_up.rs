//! Catch-up on the fake bed: a member that was offline through a vote and a
//! commit replays the retained frames and merges the missed commit through
//! the ordinary path, and a returning steward reaches the group's verdict
//! on a vote it missed instead of casting its own, and the survivor of a
//! two-member group moves it on its own authority.

mod common;

use std::time::Duration;

use common::fake_router::Bed;
use common::fast_config;
use common::net::{Frame, Welcome};
use de_mls::engine::{Action, CommitHash, Decision, Event, Phase, Verdict};

#[test]
fn a_member_offline_through_a_commit_catches_up_by_replay() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let dave = bed.seat(0, "dave");

    let es = bed.epoch_steward();
    let n = [0, bob, carol, dave]
        .into_iter()
        .find(|&i| i != 0 && i != es)
        .expect("a live node that is neither node 0 nor the epoch steward");
    let p = 0;

    let cursor = bed.now;
    bed.take_offline(n);

    bed.router_mut(p).send_chat(b"before the commit");
    bed.process(Duration::from_millis(50));

    let erin = bed.add_pending("erin");
    let key_package = bed.key_package_of(erin);
    let erin_id = bed.nodes[erin].own.clone();
    let now = bed.now;
    bed.router_mut(p)
        .announce(now, erin_id.clone(), key_package);

    let online: Vec<usize> = [0, bob, carol, dave, erin]
        .into_iter()
        .filter(|&i| i != n)
        .collect();
    bed.process_until("erin seated among the online", |b| {
        b.is_live(erin) && b.agree_among(&online)
    });

    bed.router_mut(p).send_chat(b"after the commit");
    bed.process(Duration::from_millis(50));

    let epoch_online = bed.router(p).mls.epoch();
    assert_eq!(
        bed.router(n).mls.epoch(),
        epoch_online - 1,
        "n is still one commit behind"
    );

    bed.come_back(n);

    assert!(
        bed.converged(),
        "n rejoins at the group's epoch and authenticator"
    );
    assert!(bed.router(n).mls.members().contains(&erin_id));
    assert!(
        bed.router(n)
            .events
            .iter()
            .any(|e| matches!(e, Event::MembersChanged { added, .. } if added.contains(&erin_id))),
        "n reports the member change it missed"
    );
    assert!(
        bed.router(n)
            .decisions
            .iter()
            .any(|d| matches!(d, Decision::Merge { .. })),
        "n merges the missed commit through the ordinary decision"
    );
    let chats: Vec<&[u8]> = bed
        .router(n)
        .chats
        .iter()
        .map(|(_, text)| text.as_slice())
        .collect();
    assert_eq!(
        chats,
        vec![
            b"before the commit".as_slice(),
            b"after the commit".as_slice()
        ],
        "n reads the chat it missed, in send order"
    );
    assert_eq!(
        bed.net.sent_by(n, cursor),
        0,
        "n sent nothing onto the network while it was away"
    );

    bed.router_mut(p).send_chat(b"still on the live path");
    bed.process_until("chat reaches n over the live network", |b| {
        b.router(n)
            .chats
            .iter()
            .any(|(_, text)| text.as_slice() == b"still on the live path")
    });
    bed.process_until("live again", |b| b.converged());
}

/// A member offline for only the commit itself — the vote reached it, the
/// merged commit did not — adopts it from a relayed copy on the router's
/// trust, without a decision, and then follows the next commit normally.
#[test]
fn a_member_that_missed_only_the_commit_adopts_it_and_follows_the_next() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let dave = bed.seat(0, "dave");

    let es = bed.epoch_steward();
    let n = [0, bob, carol, dave]
        .into_iter()
        .find(|&i| i != 0 && i != es)
        .expect("a live node that is neither node 0 nor the epoch steward");
    let p = 0;

    let erin = bed.add_pending("erin");
    let key_package = bed.key_package_of(erin);
    let erin_id = bed.nodes[erin].own.clone();
    let now = bed.now;
    bed.router_mut(p)
        .announce(now, erin_id.clone(), key_package);

    bed.process_until("n approved the invite", |b| {
        b.router(n).events.iter().any(|e| {
            matches!(
                e,
                Event::ConsensusReached {
                    verdict: Verdict::Approved,
                    ..
                }
            )
        })
    });

    let cursor = bed.now;
    // The steward builds only after `commit_batch_window`, several steps
    // later: taking `n` offline here leaves the commit itself unseen.
    bed.take_offline(n);

    let online: Vec<usize> = [0, bob, carol, dave, erin]
        .into_iter()
        .filter(|&i| i != n)
        .collect();
    bed.process_until("erin seated among the online", |b| {
        b.is_live(erin) && b.agree_among(&online)
    });

    let epoch_online = bed.router(p).mls.epoch();
    assert_eq!(
        bed.router(n).mls.epoch(),
        epoch_online - 1,
        "n is still one commit behind"
    );

    let commit = bed
        .net
        .since(cursor, n)
        .into_iter()
        .find_map(|(_, frame)| match frame {
            Frame::Commit(bytes) => Some(bytes),
            _ => None,
        })
        .expect("the epoch steward's commit is on the wire");

    bed.reconnect(n);
    let now = bed.now;
    bed.router_mut(n).adopt_commit(now, commit);
    let events_at_adoption = bed.router(n).events.len();
    bed.process(Duration::from_millis(50));

    assert!(
        bed.router(n)
            .events
            .iter()
            .any(|e| matches!(e, Event::CommitAdopted { .. })),
        "n reports the adoption"
    );
    assert!(
        bed.router(n).events.iter().any(|e| matches!(
            e,
            Event::MembersChanged { added, .. } if added.contains(&erin_id)
        )),
        "n reports the member change the adopted commit carried"
    );
    assert!(
        bed.converged(),
        "n rejoins at the group's epoch and authenticator"
    );

    let merges_before = bed
        .router(n)
        .decisions
        .iter()
        .filter(|d| matches!(d, Decision::Merge { .. }))
        .count();

    let frank = bed.add_pending("frank");
    let key_package = bed.key_package_of(frank);
    let frank_id = bed.nodes[frank].own.clone();
    let now = bed.now;
    bed.router_mut(p).announce(now, frank_id, key_package);
    bed.process_until("frank seated by everyone", |b| {
        b.is_live(frank) && b.converged()
    });

    assert!(
        !bed.router(n).events[events_at_adoption..]
            .iter()
            .any(|e| matches!(e, Event::CandidateRejected { .. })),
        "n follows frank's commit without rejecting it"
    );
    let merges_after = bed
        .router(n)
        .decisions
        .iter()
        .filter(|d| matches!(d, Decision::Merge { .. }))
        .count();
    assert_eq!(
        merges_after,
        merges_before + 1,
        "n merges frank's commit through the ordinary round"
    );
}

// The live member's recovery vote failed while the steward was away, one
// vote of two. The replay reaches the same verdict instead of adding the
// steward's own vote and skipping it; the steward commits the queued work.
#[test]
fn a_failed_recovery_vote_stays_failed_on_replay() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let steward = bed.epoch_steward();
    let other = if steward == 0 { bob } else { 0 };

    let carol = bed.add_pending("carol");
    let key_package = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(other).announce(now, carol_id, key_package);
    bed.process_until("both approved carol", |b| {
        [0, bob].iter().all(|&n| {
            b.router(n)
                .events
                .iter()
                .any(|e| matches!(e, Event::ConsensusReached { .. }))
        })
    });

    bed.take_offline(steward);
    bed.process_until("the miss is reported in Working", |b| {
        b.router(other)
            .events
            .iter()
            .any(|e| matches!(e, Event::CommitMissing { .. }))
            && b.router(other).engine.phase() == Phase::Working
    });
    let now = bed.now;
    bed.router_mut(other)
        .act(now, |e| e.request_recovery(now))
        .expect("recovery filed");
    bed.process_until("the recovery vote failed", |b| {
        b.router(other).events.iter().any(|e| {
            matches!(
                e,
                Event::ConsensusReached {
                    verdict: Verdict::Failed,
                    ..
                }
            )
        })
    });

    bed.come_back(steward);
    bed.process_until("carol seated everywhere", |b| {
        b.is_live(carol) && b.converged()
    });

    assert!(
        !bed.router(steward)
            .events
            .iter()
            .any(|e| matches!(e, Event::StewardSkipped { .. })),
        "the replay reached the group's verdict"
    );
    assert_eq!(bed.router(other).mls.members().len(), 3);
}

// With two members no vote can pass while one is away. The survivor
// builds the approved work on its own group and reports it as an
// adoption; the returning steward adopts the same commit.
#[test]
fn two_members_survivor_commits_on_its_own_authority() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let steward = bed.epoch_steward();
    let survivor = if steward == 0 { bob } else { 0 };

    let carol = bed.add_pending("carol");
    let key_package = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(survivor)
        .announce(now, carol_id.clone(), key_package.clone());
    bed.process_until("both approved carol", |b| {
        [0, bob].iter().all(|&n| {
            b.router(n).events.iter().any(|e| {
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

    bed.take_offline(steward);
    bed.process_until("the miss is reported in Working", |b| {
        b.router(survivor)
            .events
            .iter()
            .any(|e| matches!(e, Event::CommitMissing { .. }))
            && b.router(survivor).engine.phase() == Phase::Working
    });

    let epoch_before = bed.router(survivor).mls.epoch();
    let now = bed.now;
    let (bytes, welcome_draft) = {
        let r = bed.router_mut(survivor);
        let built = r.mls.build_commit(&[Action::Add {
            member: carol_id.clone(),
            key_package: key_package.clone(),
        }]);
        r.mls.clear_pending();
        (built.commit, built.welcome)
    };
    bed.router_mut(survivor).adopt_commit(now, bytes.clone());

    assert!(
        bed.router(survivor)
            .events
            .iter()
            .any(|e| matches!(e, Event::CommitAdopted { .. })),
        "the survivor reports its own adoption"
    );
    assert!(
        bed.router(survivor).events.iter().any(|e| matches!(
            e,
            Event::MembersChanged { added, .. } if added.contains(&carol_id)
        )),
        "the survivor reports carol's admission"
    );
    assert_eq!(bed.router(survivor).mls.epoch(), epoch_before + 1);
    assert_eq!(bed.router(survivor).mls.members().len(), 3);
    assert_eq!(bed.router(survivor).engine.phase(), Phase::Working);

    // The build bypassed the router, so its welcome is delivered by hand.
    if let Some(draft) = welcome_draft {
        let hash = CommitHash::of(&bytes);
        bed.net.broadcast(
            now,
            survivor,
            Frame::Welcome(Welcome {
                joiners: draft.joiners,
                for_commit: hash,
                snapshot: draft.snapshot,
            }),
        );
    }
    bed.process(Duration::from_millis(50));
    assert!(
        bed.is_live(carol),
        "carol picks up the hand-delivered welcome"
    );

    bed.come_back(steward);
    let now = bed.now;
    bed.router_mut(steward).adopt_commit(now, bytes);

    assert!(
        bed.router(steward)
            .events
            .iter()
            .any(|e| matches!(e, Event::CommitAdopted { .. })),
        "the steward reports its own adoption"
    );

    bed.process_until("the two original members converge", |b| {
        b.converged()
            && b.router(0).engine.phase() == Phase::Working
            && b.router(bob).engine.phase() == Phase::Working
    });

    assert_eq!(bed.router(steward).mls.members().len(), 3);
    assert_eq!(
        bed.router(steward).mls.epoch(),
        bed.router(survivor).mls.epoch()
    );
}
