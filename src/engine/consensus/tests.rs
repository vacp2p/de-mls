use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    storage::ConsensusStorage,
    types::ConsensusEvent,
};
use prost::Message;

use crate::{
    engine::{
        config::EngineConfig,
        handle::Engine,
        store::InMemoryStore,
        types::{Decision, Event, MemberId, Outbound, Output, Timestamp, Verdict},
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, MemberInvite, ViolationEvidence, conversation_update_request,
    },
};

fn member(id: u8) -> Vec<u8> {
    vec![id; 8]
}

/// A four-member engine at epoch 1, owned by member 1, ready to be
/// driven at `Timestamp::ZERO`.
fn engine() -> Engine<InMemoryStore> {
    let members: Vec<MemberId> = (1..=4).map(|i| MemberId::from(member(i))).collect();
    let (mut engine, _) = Engine::create(
        "conv",
        MemberId::from(member(1)),
        1,
        &members,
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("engine");
    engine.begin(Timestamp::ZERO);
    engine
}

fn invite_request(joiner: &[u8]) -> ConversationUpdateRequest {
    ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::MemberInvite(
            MemberInvite {
                key_package_bytes: b"kp".to_vec(),
                member_id: joiner.to_vec(),
            },
        )),
    }
}

/// A peer proposal as it arrives on the wire: opened by `owner`, still
/// well inside its expiration window.
fn peer_proposal(
    owner: &[u8],
    proposal_id: u32,
    request: &ConversationUpdateRequest,
    expected_voters: u32,
) -> Proposal {
    Proposal {
        name: format!("p{proposal_id}"),
        payload: request.encode_to_vec(),
        proposal_id,
        proposal_owner: owner.to_vec(),
        votes: Vec::new(),
        expected_voters_count: expected_voters,
        round: 1,
        timestamp: 0,
        expiration_timestamp: 600,
        liveness_criteria_yes: true,
    }
}

/// A proposal whose payload claims an owner other than the sender the
/// MLS layer authenticated is forged: it leaves no trace at all.
#[test]
fn proposal_from_a_different_sender_is_dropped() {
    let mut engine = engine();
    let request = ConversationUpdateRequest::remove_member(member(4));
    let proposal = peer_proposal(&member(2), 7, &request, 4);

    engine
        .on_incoming_proposal(&member(3), proposal)
        .expect("dropped, not an error");

    assert!(engine.out.events.is_empty());
    assert!(engine.out.decisions.is_empty());
    assert!(!engine.queues.is_voting(7));
}

/// `expected_voters == 1` bypasses peer voting, so it is allowed only
/// when the sender removes itself.
#[test]
fn single_voter_proposal_is_only_allowed_for_self_removal() {
    let mut engine = engine();
    let request = ConversationUpdateRequest::remove_member(member(4));
    let proposal = peer_proposal(&member(2), 8, &request, 1);

    engine
        .on_incoming_proposal(&member(2), proposal)
        .expect("dropped, not an error");

    assert!(!engine.queues.is_voting(8));

    let request = ConversationUpdateRequest::remove_member(member(2));
    let proposal = peer_proposal(&member(2), 9, &request, 1);
    engine
        .on_incoming_proposal(&member(2), proposal)
        .expect("self-removal accepted");
    assert!(engine.queues.is_voting(9));
}

/// An invite asks the router for a key-package verdict and arms nothing:
/// no vote request, no auto-vote. Silence must not approve a joiner
/// nobody validated.
#[test]
fn incoming_invite_asks_for_validation_before_voting() {
    let mut engine = engine();
    let request = invite_request(&member(9));
    let proposal = peer_proposal(&member(2), 11, &request, 4);

    engine.on_incoming_proposal(&member(2), proposal).unwrap();

    assert!(matches!(
        engine.out.decisions.as_slice(),
        [Decision::ValidateKeyPackage {
            proposal_id: 11,
            ..
        }]
    ));
    assert!(
        !engine
            .out
            .events
            .iter()
            .any(|e| matches!(e, Event::VoteRequested { .. }))
    );
    assert!(!engine.timing.pending_auto_votes.contains_key(&11));
    assert!(engine.timing.pending_consensus_timeouts.contains_key(&11));
}

/// A passing key package releases the vote: the request reaches the
/// router and the auto-vote timer arms.
#[test]
fn key_package_pass_releases_the_vote() {
    let mut engine = engine();
    let request = invite_request(&member(9));
    let proposal = peer_proposal(&member(2), 12, &request, 4);
    engine.on_incoming_proposal(&member(2), proposal).unwrap();
    engine.out = Output::default();

    let out = engine
        .key_package_checked(Timestamp::ZERO, 12, true)
        .unwrap();

    assert!(
        out.events
            .iter()
            .any(|e| matches!(e, Event::VoteRequested { proposal_id, .. } if *proposal_id == 12))
    );
    assert!(engine.timing.pending_auto_votes.contains_key(&12));
}

/// A failing key package votes NO right away rather than waiting the
/// session out.
#[test]
fn key_package_failure_votes_no() {
    let mut engine = engine();
    let request = invite_request(&member(9));
    let proposal = peer_proposal(&member(2), 13, &request, 4);
    engine.on_incoming_proposal(&member(2), proposal).unwrap();
    engine.out = Output::default();

    let out = engine
        .key_package_checked(Timestamp::ZERO, 13, false)
        .unwrap();

    assert!(matches!(out.outbound.as_slice(), [Outbound::Control(_)]));
    assert!(!engine.timing.pending_auto_votes.contains_key(&13));
}

/// A vote whose owner is not the authenticated sender never reaches the
/// session.
#[test]
fn vote_from_a_different_sender_is_dropped() {
    let mut engine = engine();
    let request = ConversationUpdateRequest::remove_member(member(4));
    let proposal = peer_proposal(&member(2), 14, &request, 4);
    engine.on_incoming_proposal(&member(2), proposal).unwrap();

    let vote = Vote {
        proposal_id: 14,
        vote: true,
        vote_owner: member(2),
        ..Default::default()
    };
    engine
        .forward_incoming_vote(&member(3), vote)
        .expect("dropped, not an error");

    let active = engine
        .consensus
        .storage()
        .get_active_proposals(&"conv".to_string())
        .unwrap();
    let session = active.iter().find(|p| p.proposal_id == 14).unwrap();
    assert!(session.votes.is_empty(), "forged vote must not be counted");
}

/// A proposal the local emergency freeze outranks is dropped outright —
/// not buffered, not forwarded, not surfaced.
#[test]
fn partial_freeze_drops_lower_priority_proposals() {
    let mut engine = engine();
    engine.queues.insert_emergency(1);
    let request = ConversationUpdateRequest::remove_member(member(4));
    let proposal = peer_proposal(&member(2), 15, &request, 4);

    engine.on_incoming_proposal(&member(2), proposal).unwrap();

    assert!(!engine.queues.is_voting(15));
    assert!(engine.out.events.is_empty());
}

/// A failed emergency session (the timeout passed with neither side at
/// threshold) scores nobody and lifts the partial freeze it held — the same
/// clearing a rejection gets, but with nobody to blame for the tie.
#[test]
fn failed_emergency_scores_nobody_and_lifts_the_freeze() {
    let mut engine = engine();
    let request = ViolationEvidence::deadlock(0)
        .with_creator(member(1))
        .into_update_request()
        .unwrap();
    let proposal_id = 17;
    let proposal = peer_proposal(&member(2), proposal_id, &request, 4);
    engine.on_incoming_proposal(&member(2), proposal).unwrap();
    assert!(engine.queues.has_active_emergency());
    engine.out = Output::default();

    engine
        .handle_consensus_outcome(ConsensusEvent::ConsensusFailed {
            proposal_id,
            timestamp: 0,
        })
        .unwrap();

    assert_eq!(
        engine
            .out
            .events
            .iter()
            .filter(|e| matches!(
                e,
                Event::ConsensusReached {
                    verdict: Verdict::Failed,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(
        !engine
            .out
            .events
            .iter()
            .any(|e| matches!(e, Event::MemberScoreChanged { .. }))
    );
    assert!(!engine.queues.is_voting(proposal_id));
    assert!(!engine.queues.has_active_emergency());
}

/// The auto-vote timer fires once its deadline passes and puts one Vote
/// control message on the wire.
#[test]
fn auto_vote_fires_at_its_deadline() {
    let mut engine = engine();
    let request = ConversationUpdateRequest::remove_member(member(4));
    let proposal = peer_proposal(&member(2), 16, &request, 4);
    engine.on_incoming_proposal(&member(2), proposal).unwrap();
    engine.out = Output::default();

    let fire_at = engine.timing.pending_auto_votes[&16].fire_at;
    engine.begin(fire_at);
    engine.tick_deadlines();

    assert!(matches!(
        engine.out.outbound.as_slice(),
        [Outbound::Control(_)]
    ));
    assert!(engine.timing.pending_auto_votes.is_empty());
}

/// What a resolved proposal does to the queues, and the follow-up it owes.
mod outcome_application {
    use crate::{
        engine::{
            consensus::apply::{ApplyOutcome, apply_outcome},
            queues::EngineQueues,
            types::{Event, Verdict},
        },
        peer_scoring::emergency_score_ops,
        protos::de_mls::messages::v1::{
            ConversationUpdateRequest, StewardElectionProposal, ViolationEvidence,
            conversation_update_request,
        },
        steward_list::{StewardList, StewardListConfig},
    };

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    fn election_request(stewards: Vec<Vec<u8>>, epoch: u64) -> ConversationUpdateRequest {
        ConversationUpdateRequest::steward_election(StewardElectionProposal {
            proposed_stewards: stewards,
            election_epoch: epoch,
            retry_round: 0,
        })
    }

    fn remove_request(target: Vec<u8>) -> ConversationUpdateRequest {
        ConversationUpdateRequest::remove_member(target)
    }

    fn score_below_threshold_request(
        target: Vec<u8>,
        creator: Vec<u8>,
    ) -> ConversationUpdateRequest {
        ViolationEvidence::score_below_threshold(target, 0, -10)
            .with_creator(creator)
            .into_update_request()
            .unwrap()
    }

    fn deadlock_request(creator: Vec<u8>) -> ConversationUpdateRequest {
        ViolationEvidence::deadlock(0)
            .with_creator(creator)
            .into_update_request()
            .unwrap()
    }

    /// YES on an election hands the accepted list back and leaves nothing in
    /// the approved queue.
    #[test]
    fn election_yes_returns_outcome_and_clears_voting() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mut queues = EngineQueues::new(10);
        let mems = members(&[1, 2, 3, 4, 5]);
        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-conversation", &mems, sn, config, 0).unwrap();
        let request = election_request(list.members().to_vec(), 10);

        let proposal_id = 42;
        queues.track_voting_proposal(proposal_id, &request);

        let result = apply_outcome(&mut queues, proposal_id, Verdict::Approved, &request);

        let ApplyOutcome::ElectionAccepted(outcome) = result else {
            panic!("expected ElectionAccepted, got {result:?}");
        };
        assert_eq!(outcome.election_epoch, 10);
        assert_eq!(outcome.proposed_stewards.len(), 5);
        // An election never stages into approved, and its voting entry clears.
        assert_eq!(queues.approved_proposals_count(), 0);
        assert!(!queues.is_voting(proposal_id));
    }

    /// NO or a failed session reports the drop and stages nothing.
    #[test]
    fn election_no_or_failed_returns_election_dropped() {
        for verdict in [Verdict::Rejected, Verdict::Failed] {
            let mut queues = EngineQueues::new(10);
            let request = election_request(vec![member(1), member(2)], 10);

            let proposal_id = 43;
            queues.track_voting_proposal(proposal_id, &request);

            let result = apply_outcome(&mut queues, proposal_id, verdict, &request);

            assert!(
                matches!(result, ApplyOutcome::ElectionDropped),
                "{verdict:?}"
            );
            assert_eq!(queues.approved_proposals_count(), 0, "{verdict:?}");
        }
    }

    /// A second `RemoveMember(target)` is dropped when an entry for the same
    /// target is already approved.
    #[test]
    fn removal_deduped_when_target_already_pending() {
        let mut queues = EngineQueues::new(10);
        let target = member(7);

        let first_id = 10;
        let request = remove_request(target.clone());
        let first_result = apply_outcome(&mut queues, first_id, Verdict::Approved, &request);
        assert!(matches!(first_result, ApplyOutcome::QueuedRemoval { .. }));
        assert_eq!(queues.approved_proposals_count(), 1);

        let second_id = 11;
        let request = remove_request(target.clone());
        let result = apply_outcome(&mut queues, second_id, Verdict::Approved, &request);

        assert!(matches!(result, ApplyOutcome::NoAction));
        assert_eq!(
            queues.approved_proposals_count(),
            1,
            "duplicate removal must not stack a second entry"
        );
        assert!(queues.approved_proposals().contains_key(&first_id));
        assert!(!queues.approved_proposals().contains_key(&second_id));
    }

    /// Removal dedup clears the duplicate's voting entry so the queue does
    /// not retain an outcome we deliberately discarded.
    #[test]
    fn removal_dedup_clears_voting_entry() {
        let mut queues = EngineQueues::new(10);
        let target = member(7);

        let pending_id = 20;
        queues.insert_approved_proposal(pending_id, remove_request(target.clone()));

        let dup_id = 21;
        let dup_request = remove_request(target.clone());
        queues.track_voting_proposal(dup_id, &dup_request);

        let result = apply_outcome(&mut queues, dup_id, Verdict::Approved, &dup_request);

        assert!(matches!(result, ApplyOutcome::NoAction));
        assert_eq!(queues.approved_proposals_count(), 1);
        assert!(queues.approved_proposals().contains_key(&pending_id));
        assert!(
            !queues.is_voting(dup_id),
            "duplicate must be cleared from voting queue"
        );
    }

    #[test]
    fn ecp_score_below_threshold_yes_returns_urgent_removal() {
        let mut queues = EngineQueues::new(10);
        let target = member(7);

        let request = score_below_threshold_request(target.clone(), member(1));

        let result = apply_outcome(&mut queues, 100, Verdict::Approved, &request);

        let ApplyOutcome::UrgentRemoval { target: out_target } = result else {
            panic!("expected UrgentRemoval, got {result:?}");
        };
        assert_eq!(out_target, target);
        assert_eq!(
            queues.urgent_commit_target(),
            Some(target.as_slice()),
            "urgent-commit target must be set"
        );
        assert_eq!(queues.approved_proposals_count(), 1, "RemoveMember queued");
    }

    #[test]
    fn ecp_deadlock_yes_returns_deadlock_accepted() {
        let mut queues = EngineQueues::new(10);

        let request = deadlock_request(member(1));
        let result = apply_outcome(&mut queues, 200, Verdict::Approved, &request);

        assert!(matches!(result, ApplyOutcome::DeadlockAccepted));
        assert_eq!(
            queues.approved_proposals_count(),
            0,
            "Deadlock has no specific target — no RemoveMember queued"
        );
        assert!(queues.urgent_commit_target().is_none());
    }

    /// A rejected or failed emergency does nothing at all, whatever it
    /// proposed. Every branch that acts is gated on approval, so this holds
    /// for the whole family — and it is what keeps a below-threshold removal
    /// a group decision: any member can raise one, but a NO or a tie leaves
    /// no trace, and either lifts the partial freeze it held.
    #[test]
    fn rejected_or_failed_emergency_leaves_no_trace() {
        for (name, request) in [
            ("deadlock", deadlock_request(member(1))),
            (
                "score-below-threshold",
                score_below_threshold_request(member(7), member(1)),
            ),
        ] {
            for verdict in [Verdict::Rejected, Verdict::Failed] {
                let mut queues = EngineQueues::new(10);
                queues.insert_emergency(201);
                let result = apply_outcome(&mut queues, 201, verdict, &request);

                assert!(
                    matches!(result, ApplyOutcome::NoAction),
                    "{name} {verdict:?}"
                );
                assert!(
                    queues.urgent_commit_target().is_none(),
                    "{name} {verdict:?}"
                );
                assert_eq!(queues.approved_proposals_count(), 0, "{name} {verdict:?}");
                assert!(
                    !queues.has_active_emergency(),
                    "{name} {verdict:?}: partial freeze must lift"
                );
            }
        }
    }

    /// An approved emergency lifts its own partial freeze — the freeze
    /// exists to hold the field until the session ends, not to survive it.
    #[test]
    fn approved_emergency_lifts_partial_freeze() {
        let mut queues = EngineQueues::new(10);
        queues.insert_emergency(202);
        let request = deadlock_request(member(1));

        let result = apply_outcome(&mut queues, 202, Verdict::Approved, &request);

        assert!(matches!(result, ApplyOutcome::DeadlockAccepted));
        assert!(!queues.has_active_emergency());
    }

    /// A regular `RemoveMember` reached by YES enqueues and produces no score
    /// ops.
    #[test]
    fn regular_remove_member_enqueues_without_score_ops() {
        let mut queues = EngineQueues::new(10);
        let request = remove_request(member(7));

        let proposal_id = 70;
        queues.track_voting_proposal(proposal_id, &request);

        apply_outcome(&mut queues, proposal_id, Verdict::Approved, &request);

        assert!(emergency_score_ops(&request, Verdict::Approved).is_empty());
        assert_eq!(queues.approved_proposals_count(), 1);
    }

    /// A non-score emergency approved by consensus is consumed without
    /// queuing a `RemoveMember` — only score-below-threshold ECPs transform
    /// into one.
    #[test]
    fn regular_emergency_yes_does_not_queue_remove_member() {
        let mut queues = EngineQueues::new(10);

        let request = ViolationEvidence::broken_commit(member(7), 0, Vec::<u8>::new())
            .with_creator(member(1))
            .into_update_request()
            .unwrap();

        let proposal_id = 300;
        queues.track_voting_proposal(proposal_id, &request);

        apply_outcome(&mut queues, proposal_id, Verdict::Approved, &request);

        assert_eq!(
            queues.approved_proposals_count(),
            0,
            "regular emergencies are consumed, not transformed to RemoveMember"
        );
    }

    /// An invite for a joiner already queued for admission is dropped: the
    /// group rejects a batch that admits one member twice.
    #[test]
    fn invite_deduped_when_joiner_already_queued() {
        use crate::protos::de_mls::messages::v1::MemberInvite;

        let joiner = member(9);
        let invite = |kp: &[u8]| ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::MemberInvite(
                MemberInvite {
                    key_package_bytes: kp.to_vec(),
                    member_id: joiner.clone(),
                },
            )),
        };

        let mut queues = EngineQueues::new(10);
        apply_outcome(&mut queues, 1, Verdict::Approved, &invite(b"kp-a"));
        assert_eq!(queues.approved_proposals_count(), 1);

        let result = apply_outcome(&mut queues, 2, Verdict::Approved, &invite(b"kp-b"));
        assert!(matches!(result, ApplyOutcome::NoAction));
        assert_eq!(queues.approved_proposals_count(), 1);
    }

    use crate::engine::{test_support::founder, types::Timestamp};

    /// A `Deadlock` YES skips the epoch steward for this epoch: the skip is
    /// recorded, `StewardSkipped` names it, and the rotation then hands the
    /// role to the next eligible steward.
    #[test]
    fn deadlock_accepted_skips_the_epoch_steward_and_moves_to_the_next() {
        let mut e = founder();
        e.begin(Timestamp::ZERO);
        let eligible = e.queues.steward_eligibility(&e.members);
        let es = e
            .steward_list
            .epoch_steward(e.epoch, &eligible)
            .unwrap()
            .to_vec();
        drop(eligible);

        e.skip_silent_epoch_steward();
        let out = e.finish();

        assert!(out.events.iter().any(|ev| matches!(
            ev,
            Event::StewardSkipped { steward, .. } if steward.as_bytes() == es.as_slice()
        )));
        assert!(e.queues.skipped_stewards().contains(&es));

        let eligible = e.queues.steward_eligibility(&e.members);
        let next = e.steward_list.epoch_steward(e.epoch, &eligible).unwrap();
        assert_ne!(next, es.as_slice(), "the next eligible steward takes over");
    }
}
