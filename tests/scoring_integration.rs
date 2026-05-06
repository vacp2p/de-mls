//! Integration tests for the peer-scoring pipeline at the core level
//! (no async User layer): regular-proposal no-op path and the joiner
//! default-score contract.

use prost::Message;

use de_mls::app::PeerScoringService;
use de_mls::core::{
    Group, ScoreEvent, ScoreOp, apply_consensus_result, emergency_score_ops, group_members,
};
use de_mls::ds::WELCOME_SUBTOPIC;

mod common;
use common::{
    DEFAULT_SCORE, TestMls, make_scoring, setup_joiner, setup_steward, steward_add_joiner,
};

/// Sync the scoring service's member list with the MLS group's actual members.
/// Mirrors `User::sync_scoring_members`.
fn sync_scoring_members<S: de_mls::core::PeerScoreStorage, P: de_mls::core::ScoringProvider>(
    scoring: &mut PeerScoringService<S, P>,
    group_name: &str,
    group: &Group<TestMls>,
) {
    let mls_members = group_members(group).unwrap();
    let scored = scoring.all_members_with_scores(group_name);
    let scored_ids: std::collections::HashSet<Vec<u8>> =
        scored.iter().map(|(id, _)| id.clone()).collect();
    let mls_ids: std::collections::HashSet<Vec<u8>> = mls_members.into_iter().collect();

    for member_id in &mls_ids {
        if !scored_ids.contains(member_id) {
            scoring.add_member(group_name, member_id);
        }
    }
    for member_id in &scored_ids {
        if !mls_ids.contains(member_id) {
            scoring.remove_member(group_name, member_id);
        }
    }
}

// ─────────────────────────── Tests ───────────────────────────

/// Non-emergency proposal produces no score ops.
#[test]
fn test_scoring_no_ops_for_regular_proposal() {
    let group_name = "scoring-regular";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    let mut alice_handle = setup_steward(group_name, alice_hex);

    let regular_request = de_mls::protos::de_mls::messages::v1::GroupUpdateRequest {
        payload: Some(
            de_mls::protos::de_mls::messages::v1::group_update_request::Payload::RemoveMember(
                de_mls::protos::de_mls::messages::v1::RemoveMember {
                    identity: vec![0xCC],
                },
            ),
        ),
    };

    let payload = regular_request.encode_to_vec();
    let proposal_id = 70;
    alice_handle.store_voting_proposal(proposal_id, regular_request);

    apply_consensus_result(&mut alice_handle, proposal_id, true, &payload).unwrap();
    assert!(emergency_score_ops(&payload, true).is_empty());

    // Proposal should be in approved queue (ready for commit).
    assert_eq!(alice_handle.approved_proposals_count(), 1);
}

/// Core-level contract: a joiner's scoring starts from MLS membership and
/// has no way to reconstruct peers' prior scores at this layer. The
/// User-layer `GroupSync` app message closes that gap separately.
#[test]
fn test_new_joiner_starts_with_default_scores() {
    let group_name = "scoring-join-gap";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let bob_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let mut alice_handle = setup_steward(group_name, alice_hex);
    let alice_id = alice_handle.self_identity().to_vec();

    // Alice's scoring has her at a non-default score (simulating prior events).
    let mut alice_scoring = make_scoring();
    alice_scoring.add_member(group_name, &alice_id);
    alice_scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: alice_id.clone(),
            event: ScoreEvent::EmergencyYesCreator,
        },
    );
    assert_eq!(
        alice_scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE + 20)
    );

    // Bob joins.
    let mut bob = setup_joiner(group_name, bob_hex);
    let (welcome, _batch) = steward_add_joiner(&mut alice_handle, &bob.kp_packet);
    bob.accept_welcome_packet(&welcome);
    // Sanity-check that the welcome path still surfaces only the
    // welcome subtopic at this layer; nothing else flows through compat.
    assert_eq!(welcome.subtopic, WELCOME_SUBTOPIC);

    // Bob builds his scoring from MLS members — gets defaults.
    let mut bob_scoring = make_scoring();
    sync_scoring_members(&mut bob_scoring, group_name, &bob.group);

    assert_eq!(
        bob_scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE),
    );
}
