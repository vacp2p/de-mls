use std::time::Duration;

use prost::Message;

use super::select::{CandidateVerdict, actions_match_voted};
use crate::{
    Action, ScoreEvent,
    engine::{
        commit::buffer::{BufferedCandidate, CommitRoundBuffer},
        config::EngineConfig,
        handle::{Engine, PendingMerge},
        store::InMemoryStore,
        types::{
            Admission, CommitHash, Decision, DecisionFailure, Event, MemberId, Outbound,
            StagedFacts, Timestamp,
        },
    },
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, MemberInvite, RemoveMember,
        conversation_update_request::Payload,
    },
};

fn at(secs: u64) -> Timestamp {
    Timestamp::from_duration_since_epoch(Duration::from_secs(secs))
}

fn id(name: &str) -> MemberId {
    MemberId::from(name.as_bytes())
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
            member_id: joiner.as_bytes().to_vec(),
        })),
    }
}

fn removal(target: &str) -> ConversationUpdateRequest {
    ConversationUpdateRequest {
        payload: Some(Payload::RemoveMember(RemoveMember {
            member_id: target.as_bytes().to_vec(),
        })),
    }
}

fn envelope(steward: &str, count: u32, commit: &[u8]) -> CommitCandidate {
    CommitCandidate {
        conversation_id: b"conv".to_vec(),
        commit_message: commit.to_vec(),
        steward_member_id: steward.as_bytes().to_vec(),
        proposal_count: count,
    }
}

fn remote(steward: &str, count: u32, commit: &[u8]) -> BufferedCandidate {
    BufferedCandidate {
        hash: CommitHash::of(commit),
        envelope: envelope(steward, count, commit),
        facts: Some(StagedFacts {
            sender: id(steward),
            epoch: 0,
            actions: Vec::new(),
            proposal_count: count,
            self_removed: false,
        }),
        is_local: false,
    }
}

// ── the buffer ─────────────────────────────────────────────────────

#[test]
fn the_buffer_dedupes_by_hash_and_caps_at_one_per_member() {
    let mut buffer = CommitRoundBuffer::default();
    assert!(buffer.insert(0, remote("a", 1, b"aa"), 2));
    assert!(
        !buffer.insert(0, remote("a", 1, b"aa"), 2),
        "the same commit twice is one candidate"
    );
    assert!(buffer.insert(0, remote("b", 1, b"bb"), 2));
    assert!(
        !buffer.insert(0, remote("c", 1, b"cc"), 2),
        "beyond one per member a distinct candidate is a fork"
    );
    assert_eq!(buffer.candidates(0).len(), 2);
}

#[test]
fn a_new_epoch_starts_a_fresh_round() {
    let mut buffer = CommitRoundBuffer::default();
    buffer.insert(0, remote("a", 1, b"aa"), 2);
    buffer.await_facts(0, CommitHash::of(b"bb"), envelope("b", 1, b"bb"));
    buffer.insert(1, remote("c", 1, b"cc"), 2);
    assert!(buffer.candidates(0).is_empty(), "epoch 0 is stale");
    assert_eq!(buffer.candidates(1).len(), 1);
    assert!(buffer.take_awaited(&CommitHash::of(b"bb")).is_none());
}

#[test]
fn only_our_own_candidate_counts_as_local() {
    let mut buffer = CommitRoundBuffer::default();
    buffer.insert(0, remote("a", 1, b"aa"), 4);
    assert!(buffer.candidates(0).iter().all(|c| !c.is_local));
    let mut ours = remote("b", 1, b"bb");
    ours.is_local = true;
    ours.facts = None;
    buffer.insert(0, ours, 4);
    assert!(buffer.candidates(0).iter().any(|c| c.is_local));
}

// ── admission ──────────────────────────────────────────────────────

#[test]
fn admission_takes_a_well_formed_envelope_and_drops_the_rest() {
    let mut e = engine(&["alice", "bob"]);
    let good = envelope("bob", 1, b"commit").encode_to_vec();
    assert_eq!(
        e.admit_candidate(at(0), &good).unwrap(),
        Admission::Stage {
            hash: CommitHash::of(b"commit"),
            commit: b"commit".to_vec(),
        }
    );
    assert_eq!(
        e.admit_candidate(at(0), &good).unwrap(),
        Admission::Drop,
        "a second sighting of the same commit is already in the round"
    );

    let mut e = engine(&["alice", "bob"]);
    assert_eq!(
        e.admit_candidate(at(0), b"not a proto\xff\xff").unwrap(),
        Admission::Drop
    );
    assert_eq!(
        e.admit_candidate(at(0), &envelope("bob", 0, b"commit").encode_to_vec())
            .unwrap(),
        Admission::Drop,
        "a candidate claiming no proposals commits nothing"
    );
    assert_eq!(
        e.admit_candidate(at(0), &envelope("bob", 1, b"").encode_to_vec())
            .unwrap(),
        Admission::Drop
    );
    assert_eq!(
        e.admit_candidate(at(0), &envelope("", 1, b"commit").encode_to_vec())
            .unwrap(),
        Admission::Drop
    );
    let mut other = envelope("bob", 1, b"commit");
    other.conversation_id = b"elsewhere".to_vec();
    assert_eq!(
        e.admit_candidate(at(0), &other.encode_to_vec()).unwrap(),
        Admission::Drop
    );
}

#[test]
fn an_already_committed_commit_is_never_re_admitted() {
    let mut e = engine(&["alice", "bob"]);
    e.queues.insert_committed_hash(CommitHash::of(b"commit"));
    assert_eq!(
        e.admit_candidate(at(0), &envelope("bob", 1, b"commit").encode_to_vec())
            .unwrap(),
        Admission::Drop
    );
}

#[test]
fn a_candidate_ahead_of_its_approval_is_stashed_and_replayed() {
    let mut e = engine(&["alice", "bob"]);
    let admitted = e
        .admit_candidate(at(0), &envelope("bob", 1, b"commit").encode_to_vec())
        .unwrap();
    let Admission::Stage { hash, .. } = admitted else {
        panic!("expected a staging admission, got {admitted:?}");
    };
    let facts = StagedFacts {
        sender: id("bob"),
        epoch: 0,
        actions: vec![Action::Add {
            member: id("carol"),
            key_package: b"kp:carol".to_vec(),
        }],
        proposal_count: 1,
        self_removed: false,
    };
    e.handle_candidate(at(0), hash, facts).unwrap();
    assert_eq!(
        e.commit_candidate_count().0,
        0,
        "nothing approved yet, so the candidate waits outside the round"
    );

    e.queues.insert_approved_proposal(7, invite("carol"));
    e.replay_early_candidates().unwrap();
    assert_eq!(e.commit_candidate_count().0, 1);
}

// ── the validation ladder ──────────────────────────────────────────

/// A claimed count that contradicts the staged commit is a violation
/// pinned on the authenticated signer; an honest claim passes.
#[test]
fn a_lying_proposal_count_is_a_violation_and_an_honest_one_passes() {
    let mut e = engine(&["alice", "bob"]);
    e.queues.insert_approved_proposal(7, invite("carol"));
    let action = Action::Add {
        member: id("carol"),
        key_package: b"kp:carol".to_vec(),
    };

    let mut lying = remote("bob", 2, b"commit");
    lying.facts = Some(StagedFacts {
        sender: id("bob"),
        epoch: 0,
        actions: vec![action.clone()],
        proposal_count: 1,
        self_removed: false,
    });
    match e.verdict(&lying) {
        CandidateVerdict::Reject(Some(op)) => {
            assert_eq!(op.member_id, b"bob");
            assert_eq!(op.event, ScoreEvent::BrokenCommit);
        }
        _ => panic!("a claim that contradicts the commit must be rejected"),
    }

    let mut honest = remote("bob", 1, b"commit");
    honest.facts = Some(StagedFacts {
        sender: id("bob"),
        epoch: 0,
        actions: vec![action],
        proposal_count: 1,
        self_removed: false,
    });
    assert!(matches!(e.verdict(&honest), CandidateVerdict::Accept));
}

/// The wire id is forgeable, so it is checked against the signer and the
/// penalty follows the signer.
#[test]
fn a_forged_steward_id_is_pinned_on_the_signer() {
    let mut e = engine(&["alice", "bob"]);
    e.queues.insert_approved_proposal(7, invite("carol"));
    let mut forged = remote("alice", 1, b"commit");
    forged.facts = Some(StagedFacts {
        sender: id("bob"),
        epoch: 0,
        actions: Vec::new(),
        proposal_count: 1,
        self_removed: false,
    });
    match e.verdict(&forged) {
        CandidateVerdict::Reject(Some(op)) => {
            assert_eq!(op.member_id, b"bob", "the signer, not the wire claim");
            assert_eq!(op.event, ScoreEvent::BrokenCommit);
        }
        _ => panic!("a forged steward id must be rejected"),
    }
}

/// A commit carrying proposal kinds the engine cannot read has no
/// checkable action set.
#[test]
fn a_commit_with_unreadable_proposals_is_rejected() {
    let mut e = engine(&["alice", "bob"]);
    e.queues.insert_approved_proposal(7, invite("carol"));
    let mut opaque = remote("bob", 2, b"commit");
    opaque.facts = Some(StagedFacts {
        sender: id("bob"),
        epoch: 0,
        actions: vec![Action::Add {
            member: id("carol"),
            key_package: b"kp:carol".to_vec(),
        }],
        proposal_count: 2,
        self_removed: false,
    });
    match e.verdict(&opaque) {
        CandidateVerdict::Reject(Some(op)) => {
            assert_eq!(op.event, ScoreEvent::BrokenMlsProposal)
        }
        _ => panic!("a commit the round cannot read must be rejected"),
    }
}

/// The actions must be exactly the voted set, keyed the same on both
/// sides.
#[test]
fn actions_beyond_the_voted_set_are_rejected() {
    let mut e = engine(&["alice", "bob"]);
    e.queues.insert_approved_proposal(7, invite("carol"));
    let mut extra = remote("bob", 2, b"commit");
    extra.envelope.proposal_count = 2;
    extra.facts = Some(StagedFacts {
        sender: id("bob"),
        epoch: 0,
        actions: vec![
            Action::Add {
                member: id("carol"),
                key_package: b"kp:carol".to_vec(),
            },
            Action::Remove {
                member: id("alice"),
            },
        ],
        proposal_count: 2,
        self_removed: true,
    });
    match e.verdict(&extra) {
        CandidateVerdict::Reject(Some(op)) => {
            assert_eq!(op.event, ScoreEvent::BrokenMlsProposal)
        }
        _ => panic!("actions beyond the voted set must be rejected"),
    }
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

#[test]
fn our_own_candidate_wins_and_the_merge_report_moves_the_view() {
    let mut e = engine(&["alice"]);
    e.queues.insert_approved_proposal(7, invite("bob"));
    e.begin(at(0));
    assert!(e.request_own_candidate().unwrap());
    assert_eq!(
        e.pending_build.as_ref().map(|b| b.actions.clone()),
        Some(vec![Action::Add {
            member: id("bob"),
            key_package: b"kp:bob".to_vec(),
        }])
    );

    let hash = CommitHash::of(b"commit");
    let out = e
        .own_candidate_built(at(0), hash, 1, b"commit".to_vec())
        .unwrap();
    assert!(matches!(out.outbound.first(), Some(Outbound::Candidate(_))));

    let winner = e
        .select_epoch_steward_candidate()
        .unwrap()
        .expect("our own candidate wins");
    assert!(winner.is_local && winner.losers.is_empty());
    e.decide_winner(winner);

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
    assert!(e.queues.has_committed_hash(&hash));
    assert_eq!(e.commit_candidate_count().0, 0, "the round is over");
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

/// A candidate from a non-epoch-steward is rejected and scored
/// `MisbehavingCommit`, even when its actions exactly match the voted
/// set — only the epoch steward's own candidate may win a round.
#[test]
fn a_non_epoch_steward_candidate_is_rejected_even_with_matching_actions() {
    let mut e = engine(&["alice", "bob", "carol"]);
    e.queues.insert_approved_proposal(7, invite("dave"));
    let es = e.expected_steward().unwrap();
    let impostor = ["alice", "bob", "carol"]
        .into_iter()
        .find(|m| id(m).as_bytes() != es.as_slice())
        .expect("at least one member isn't the epoch steward");
    let action = Action::Add {
        member: id("dave"),
        key_package: b"kp:dave".to_vec(),
    };
    let mut candidate = remote(impostor, 1, b"commit");
    candidate.facts = Some(StagedFacts {
        sender: id(impostor),
        epoch: 0,
        actions: vec![action],
        proposal_count: 1,
        self_removed: false,
    });
    e.round.insert(0, candidate, 3);

    let before = e.scoring.score_for(impostor.as_bytes()).unwrap();
    let winner = e.select_epoch_steward_candidate().unwrap();
    assert!(winner.is_none(), "no epoch-steward candidate was submitted");
    let after = e.scoring.score_for(impostor.as_bytes()).unwrap();
    assert!(after < before, "the impostor is penalised");
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
    assert_eq!(e.phase(), crate::engine::types::Phase::Working);
}

/// A merge the router could not execute closes the round with no commit:
/// no second merge is asked for, and the approved batch stays queued for
/// the next round.
#[test]
fn a_failed_merge_closes_the_round_without_a_commit() {
    let mut e = engine(&["alice", "bob", "carol"]);
    e.queues.insert_approved_proposal(7, invite("dave"));
    let es = String::from_utf8(e.expected_steward().unwrap()).unwrap();
    let action = Action::Add {
        member: id("dave"),
        key_package: b"kp:dave".to_vec(),
    };
    let commit = format!("commit:{es}").into_bytes();
    let mut candidate = remote(&es, 1, &commit);
    candidate.facts = Some(StagedFacts {
        sender: id(&es),
        epoch: 0,
        actions: vec![action],
        proposal_count: 1,
        self_removed: false,
    });
    let hash = candidate.hash;
    e.round.insert(0, candidate, 3);

    e.begin(at(0));
    e.start_freezing();
    e.start_selection();
    let winner = e
        .select_epoch_steward_candidate()
        .unwrap()
        .expect("the epoch steward's candidate wins");
    assert_eq!(winner.hash, hash);
    e.decide_winner(winner);
    let _ = e.finish();

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
        Event::CommitMissing { steward, .. } if steward == &Some(id(&es))
    )));
    assert_eq!(e.phase(), crate::engine::types::Phase::Working);
    assert_eq!(
        e.queues.approved_proposals_count(),
        1,
        "the approved batch stays queued for the next round"
    );
}
