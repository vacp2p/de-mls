use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    storage::ConsensusStorage,
};
use prost::Message;

use crate::{
    engine::{
        config::EngineConfig,
        handle::Engine,
        store::InMemoryStore,
        types::{Decision, Event, MemberId, Outbound, Output, Timestamp},
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, MemberInvite, conversation_update_request,
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
