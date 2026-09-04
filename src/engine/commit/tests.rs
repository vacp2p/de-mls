use std::time::Duration;

use super::select::actions_match_voted;
use crate::{
    Action,
    engine::{
        config::EngineConfig,
        handle::{Engine, PendingMerge},
        store::InMemoryStore,
        test_support::{id, member},
        types::{
            CommitHash, Decision, DecisionFailure, Event, MemberId, Phase, StagedFacts, Timestamp,
        },
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, MemberInvite, RemoveMember, conversation_update_request::Payload,
    },
};

fn at(secs: u64) -> Timestamp {
    Timestamp::from_duration_since_epoch(Duration::from_secs(secs))
}

/// An engine over `members`, the first of which is this node. `create`
/// installs the steward list over all of them at epoch 0.
fn engine(members: &[&str]) -> Engine<InMemoryStore> {
    let ids: Vec<MemberId> = members.iter().map(|m| id(m)).collect();
    let (engine, _) = Engine::<InMemoryStore>::create(
        "conv",
        ids[0].clone(),
        0,
        &ids,
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create");
    engine
}

fn invite(joiner: &str) -> ConversationUpdateRequest {
    ConversationUpdateRequest {
        payload: Some(Payload::MemberInvite(MemberInvite {
            key_package_bytes: format!("kp:{joiner}").into_bytes(),
            member_id: member(joiner),
        })),
    }
}

fn removal(target: &str) -> ConversationUpdateRequest {
    ConversationUpdateRequest {
        payload: Some(Payload::RemoveMember(RemoveMember {
            member_id: member(target),
        })),
    }
}

// ── admission at handle_candidate ───────────────────────────────────

/// A candidate sealed at any epoch but the current one is discarded on
/// sight, with no score against its sender: staleness is not misbehaviour.
#[test]
fn a_stale_epoch_candidate_is_discarded_without_a_score() {
    let mut e = engine(&["alice", "bob"]);
    let before = e.scoring.score_for(&member("bob")).unwrap();
    let hash = CommitHash::of(b"commit");
    let out = e
        .handle_candidate(
            at(0),
            hash,
            StagedFacts {
                sender: id("bob"),
                epoch: e.epoch + 5,
                actions: Vec::new(),
                proposal_count: 0,
                self_removed: false,
            },
        )
        .unwrap();
    assert_eq!(
        out.decisions,
        vec![Decision::Discard { hashes: vec![hash] }]
    );
    let after = e.scoring.score_for(&member("bob")).unwrap();
    assert_eq!(before, after, "staleness alone scores nothing");
}

/// A candidate from anyone but the epoch steward is discarded on sight and
/// its sender scored `MisbehavingCommit`, whatever it carries.
#[test]
fn a_non_epoch_steward_candidate_is_discarded_and_scored() {
    let mut e = engine(&["alice", "bob", "carol"]);
    let es = e.expected_steward().unwrap();
    let impostor = ["alice", "bob", "carol"]
        .into_iter()
        .find(|m| id(m).as_bytes() != es.as_slice())
        .expect("at least one member isn't the epoch steward");

    let before = e.scoring.score_for(&member(impostor)).unwrap();
    let hash = CommitHash::of(b"commit");
    let out = e
        .handle_candidate(
            at(0),
            hash,
            StagedFacts {
                sender: id(impostor),
                epoch: 0,
                actions: Vec::new(),
                proposal_count: 0,
                self_removed: false,
            },
        )
        .unwrap();
    assert_eq!(
        out.decisions,
        vec![Decision::Discard { hashes: vec![hash] }]
    );
    let after = e.scoring.score_for(&member(impostor)).unwrap();
    assert!(after < before, "the impostor is penalised");
}

/// The voted set and the commit are compared as deduplicated sets: two
/// approvals naming one change are committed once.
#[test]
fn duplicate_approvals_of_one_change_match_a_single_action() {
    let mut e = engine(&["alice", "bob"]);
    e.queues.insert_approved_proposal(7, invite("carol"));
    e.queues.insert_approved_proposal(8, invite("carol"));
    assert!(actions_match_voted(
        &e.queues,
        &[Action::Add {
            member: id("carol"),
            key_package: b"kp:carol".to_vec(),
        }]
    ));
}

// ── the round end to end ───────────────────────────────────────────

/// The epoch steward's candidate sits in `round_candidate` until the round
/// is closed; it merges only then, not the instant `handle_candidate`
/// accepts it.
#[test]
fn the_epoch_stewards_candidate_merges_only_at_window_close() {
    let mut e = engine(&["alice"]);
    e.queues.insert_approved_proposal(7, invite("bob"));
    e.begin(at(0));
    let event = e.start_freezing().expect("enters freezing");
    e.on_freeze_entered(event).unwrap();

    let hash = CommitHash::of(b"commit");
    e.handle_candidate(
        at(0),
        hash,
        StagedFacts {
            sender: id("alice"),
            epoch: 0,
            actions: vec![Action::Add {
                member: id("bob"),
                key_package: b"kp:bob".to_vec(),
            }],
            proposal_count: 1,
            self_removed: false,
        },
    )
    .unwrap();
    assert!(e.round_candidate.is_some(), "held, not merged yet");

    e.start_selection();
    e.close_round().unwrap();
    let out = e.finish();
    assert!(
        out.decisions
            .iter()
            .any(|d| matches!(d, Decision::Merge { hash: h } if *h == hash)),
        "merges once the round closes"
    );
}

/// The steward's own commit, reported back through `handle_candidate` the
/// same way a peer's is, merges with `pending_merge.is_local` set.
#[test]
fn the_own_commit_reported_through_handle_candidate_merges() {
    let mut e = engine(&["alice"]);
    e.queues.insert_approved_proposal(7, invite("bob"));
    e.begin(at(0));
    assert!(e.request_own_candidate().unwrap());

    let hash = CommitHash::of(b"commit");
    let out = e
        .handle_candidate(
            at(0),
            hash,
            StagedFacts {
                sender: id("alice"),
                epoch: 0,
                actions: vec![Action::Add {
                    member: id("bob"),
                    key_package: b"kp:bob".to_vec(),
                }],
                proposal_count: 1,
                self_removed: false,
            },
        )
        .unwrap();
    assert!(out.events.contains(&Event::CommitRoundProgress {
        received: 1,
        expected: 1,
    }));

    e.start_selection();
    e.close_round().unwrap();
    assert!(e.pending_merge.as_ref().is_some_and(|m| m.is_local));
    let out = e.finish();
    assert!(matches!(
        out.decisions.as_slice(),
        [Decision::Merge { hash: h }] if *h == hash
    ));

    let out = e
        .commit_applied(at(0), hash, 1, &[id("alice"), id("bob")])
        .unwrap();
    assert_eq!(e.epoch(), 1);
    assert!(e.members().contains(&id("bob")));
    assert!(
        out.events.iter().any(|ev| matches!(
            ev,
            Event::MembersChanged { added, removed }
                if added == &vec![id("bob")] && removed.is_empty()
        )),
        "the seated joiner is reported once, got {:?}",
        out.events
    );
    assert_eq!(
        e.queues.approved_proposals_count(),
        0,
        "the committed batch leaves the queue"
    );
}

/// A merge report for a commit the engine never decided still moves the
/// view — the group is the truth — but says so.
#[test]
fn an_unexpected_merge_is_adopted_and_reported() {
    let mut e = engine(&["alice", "bob"]);
    let out = e
        .commit_applied(at(0), CommitHash::of(b"surprise"), 4, &[id("alice")])
        .unwrap();
    assert_eq!(e.epoch(), 4);
    assert_eq!(e.members(), vec![id("alice")]);
    assert!(out.events.iter().any(|ev| matches!(
        ev,
        Event::Error { operation, .. } if operation == "commit_applied"
    )));
}

/// A commit that unseats us ends the conversation: no steward work, one
/// `Leave`.
#[test]
fn a_commit_that_removes_us_asks_the_router_to_leave() {
    let mut e = engine(&["alice", "bob"]);
    e.queues.insert_approved_proposal(7, removal("alice"));
    let hash = CommitHash::of(b"commit");
    e.pending_merge = Some(PendingMerge {
        hash,
        facts: Some(StagedFacts {
            sender: id("bob"),
            epoch: 0,
            actions: vec![Action::Remove {
                member: id("alice"),
            }],
            proposal_count: 1,
            self_removed: true,
        }),
        is_local: false,
    });
    let out = e.commit_applied(at(0), hash, 1, &[id("bob")]).unwrap();
    assert_eq!(out.decisions, vec![Decision::Leave]);
}

/// A silent epoch steward — no candidate submitted before the round
/// closes — yields `CommitMissing` naming it, keeps the approved batch
/// queued, and scores it `CensorshipInactivity`.
#[test]
fn a_silent_epoch_steward_yields_commit_missing_and_keeps_the_batch() {
    let mut e = engine(&["alice", "bob"]);
    if e.expected_steward().unwrap() == e.own {
        // The member set is the same either way; swapping which one is
        // `own` moves the deterministic epoch steward off us.
        e = engine(&["bob", "alice"]);
    }
    e.queues.insert_approved_proposal(7, invite("carol"));
    let es = e.expected_steward().unwrap();
    assert_ne!(es, e.own, "the test needs a steward other than us");

    e.begin(at(0));
    let before = e.scoring.score_for(&es).unwrap();
    e.close_round_without_commit().unwrap();
    let out = e.finish();

    assert!(out.events.iter().any(|ev| matches!(
        ev,
        Event::CommitMissing { steward, .. } if steward == &Some(MemberId::from(es.as_slice()))
    )));
    let after = e.scoring.score_for(&es).unwrap();
    assert!(after < before, "the silent steward is penalised");
    assert_eq!(
        e.queues.approved_proposals_count(),
        1,
        "the approved batch stays queued"
    );
    assert_eq!(e.phase(), Phase::Working);
}

/// A candidate whose actions don't match the voted set is discarded and its
/// sender scored `BrokenMlsProposal`, and the round then reports the same
/// miss it would for an absent candidate.
#[test]
fn a_mismatching_candidate_is_discarded_and_commit_missing_follows() {
    let mut e = engine(&["alice"]);
    e.queues.insert_approved_proposal(7, invite("bob"));
    let hash = CommitHash::of(b"commit");
    e.handle_candidate(
        at(0),
        hash,
        StagedFacts {
            sender: id("alice"),
            epoch: 0,
            actions: vec![Action::Add {
                member: id("carol"),
                key_package: b"kp:carol".to_vec(),
            }],
            proposal_count: 1,
            self_removed: false,
        },
    )
    .unwrap();

    e.start_selection();
    let before = e.scoring.score_for(&member("alice")).unwrap();
    e.close_round().unwrap();
    let out = e.finish();

    assert!(
        out.decisions
            .iter()
            .any(|d| matches!(d, Decision::Discard { hashes } if hashes == &vec![hash])),
        "the mismatching commit is discarded"
    );
    assert!(out.events.iter().any(|ev| matches!(
        ev,
        Event::CommitMissing { steward, .. } if steward == &Some(id("alice"))
    )));
    let after = e.scoring.score_for(&member("alice")).unwrap();
    assert!(after < before, "the sender is penalised for the mismatch");
    assert_eq!(e.phase(), Phase::Working);
    assert_eq!(
        e.queues.approved_proposals_count(),
        1,
        "the approved batch stays queued for the next round"
    );
}

/// A merge the router could not execute closes the round with no commit:
/// no second merge is asked for, and the approved batch stays queued for
/// the next round.
#[test]
fn a_failed_merge_closes_the_round_without_a_commit() {
    let mut e = engine(&["alice", "bob", "carol"]);
    e.queues.insert_approved_proposal(7, invite("dave"));
    let es = e.expected_steward().unwrap();
    let action = Action::Add {
        member: id("dave"),
        key_package: b"kp:dave".to_vec(),
    };
    let hash = CommitHash::of(b"commit");

    e.begin(at(0));
    e.start_freezing();
    e.handle_candidate(
        at(0),
        hash,
        StagedFacts {
            sender: MemberId::from(es.as_slice()),
            epoch: 0,
            actions: vec![action],
            proposal_count: 1,
            self_removed: false,
        },
    )
    .unwrap();
    e.start_selection();
    e.close_round().unwrap();
    let _ = e.finish();
    assert!(e.pending_merge.is_some());

    let out = e
        .decision_failed(
            at(0),
            DecisionFailure {
                decision: Decision::Merge { hash },
                reason: "no such staged commit".to_owned(),
            },
        )
        .unwrap();
    assert!(
        !out.decisions
            .iter()
            .any(|d| matches!(d, Decision::Merge { .. })),
        "no second merge is asked for"
    );
    assert!(e.pending_merge.is_none());
    assert!(out.events.iter().any(|ev| matches!(
        ev,
        Event::CommitMissing { steward, .. } if steward == &Some(MemberId::from(es.as_slice()))
    )));
    assert_eq!(e.phase(), Phase::Working);
    assert_eq!(
        e.queues.approved_proposals_count(),
        1,
        "the approved batch stays queued for the next round"
    );
}
