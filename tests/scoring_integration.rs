//! Integration test for the peer scoring pipeline.
//!
//! Verifies the full flow: group creation → member join → scoring init from MLS
//! members → emergency proposal → score ops applied → scores updated.
//!
//! This test operates at the core + scoring service level, without the async
//! User layer, to keep the test focused and synchronous.

use prost::Message;

use de_mls::app::{PeerScoringService, emergency_score_ops};
use de_mls::core::{
    Group, ProcessResult, ProtocolConfig, ScoreEvent, ScoreOp, apply_consensus_result,
    build_key_package_message, create_group, group_members, prepare_to_join, process_inbound,
};
use de_mls::ds::WELCOME_SUBTOPIC;
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService};
use de_mls::protos::de_mls::messages::v1::ViolationEvidence;

mod common;
use common::{DEFAULT_SCORE, make_scoring, setup_mls, steward_add_joiner};

/// Sync the scoring service's member list with the MLS group's actual members.
/// Mirrors `User::sync_scoring_members`.
fn sync_scoring_members<S: de_mls::core::PeerScoreStorage, P: de_mls::core::ScoringProvider>(
    scoring: &mut PeerScoringService<S, P>,
    group_name: &str,
    group: &Group,
    mls: &MlsService<MemoryDeMlsStorage>,
) {
    let mls_members = group_members(group, mls).unwrap();
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

/// Full pipeline: create group → steward in scoring → add joiner →
/// sync members → both at default → emergency accepted → scores change.
#[test]
fn test_scoring_pipeline_create_join_emergency() {
    let group_name = "scoring-pipeline";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let bob_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    // ── Step 1: Alice creates group ──
    let alice_mls = setup_mls(alice_hex);
    let mut alice_handle =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    let alice_id = alice_mls.wallet_bytes();

    let mut scoring = make_scoring();

    // Register creator in scoring (mirrors User::create_group)
    scoring.add_member(group_name, &alice_id);

    assert_eq!(scoring.all_members_with_scores(group_name).len(), 1);
    assert_eq!(
        scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE)
    );

    // ── Step 2: Bob joins ──
    let bob_mls = setup_mls(bob_hex);
    let mut bob_handle = prepare_to_join(
        group_name,
        bob_mls.wallet_bytes(),
        ProtocolConfig::new(1, 5).unwrap(),
    );
    let bob_kp = build_key_package_message(&bob_handle, &bob_mls, b"test-app-id").unwrap();
    let bob_id = bob_mls.wallet_bytes();

    let (welcome, _batch) = steward_add_joiner(&alice_mls, &mut alice_handle, &bob_kp);

    // Steward side: sync after GroupUpdated
    sync_scoring_members(&mut scoring, group_name, &alice_handle, &alice_mls);

    assert_eq!(scoring.all_members_with_scores(group_name).len(), 2);
    assert_eq!(
        scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE)
    );
    assert_eq!(scoring.score_for(group_name, &bob_id), Some(DEFAULT_SCORE));

    // Bob processes welcome
    let join_result = process_inbound(
        &mut bob_handle,
        &welcome.payload,
        WELCOME_SUBTOPIC,
        &bob_mls,
    )
    .unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    // Bob's scoring: sync from MLS group members (mirrors User::dispatch_inbound_result JoinedGroup)
    let mut bob_scoring = make_scoring();
    sync_scoring_members(&mut bob_scoring, group_name, &bob_handle, &bob_mls);

    // Bob should see both members with default scores
    assert_eq!(bob_scoring.all_members_with_scores(group_name).len(), 2);
    assert_eq!(
        bob_scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE)
    );
    assert_eq!(
        bob_scoring.score_for(group_name, &bob_id),
        Some(DEFAULT_SCORE)
    );

    // ── Step 3: Emergency proposal ACCEPTED (Bob accuses Alice) ──
    // Bob creates an ECP targeting Alice
    let emergency = ViolationEvidence::broken_commit(alice_id.clone(), 0, Vec::<u8>::new())
        .with_creator(bob_id.clone())
        .into_update_request()
        .unwrap();
    let payload = emergency.encode_to_vec();

    // Bob is the owner (creator of the proposal)
    let proposal_id = 50;
    bob_handle.store_voting_proposal(proposal_id, emergency.clone());

    // Bob's node: approved owner (payload available from consensus service)
    apply_consensus_result(&mut bob_handle, proposal_id, true, &payload).unwrap();
    let bob_score_ops = emergency_score_ops(&payload, true);
    assert_eq!(bob_score_ops.len(), 2);
    bob_scoring.apply_ops(group_name, &bob_score_ops);

    // Alice's node: approved non-owner (she receives the payload)
    apply_consensus_result(&mut alice_handle, proposal_id, true, &payload).unwrap();
    let alice_score_ops = emergency_score_ops(&payload, true);
    assert_eq!(alice_score_ops.len(), 2);
    scoring.apply_ops(group_name, &alice_score_ops);

    // Verify scores: Alice penalized (target), Bob rewarded (creator)
    // Both nodes should agree on scores.
    let alice_score_on_alice_node = scoring.score_for(group_name, &alice_id).unwrap();
    let bob_score_on_alice_node = scoring.score_for(group_name, &bob_id).unwrap();
    let alice_score_on_bob_node = bob_scoring.score_for(group_name, &alice_id).unwrap();
    let bob_score_on_bob_node = bob_scoring.score_for(group_name, &bob_id).unwrap();

    assert_eq!(alice_score_on_alice_node, DEFAULT_SCORE - 50); // BrokenCommit
    assert_eq!(bob_score_on_alice_node, DEFAULT_SCORE + 20); // EmergencyYesCreator
    assert_eq!(alice_score_on_bob_node, DEFAULT_SCORE - 50); // Same
    assert_eq!(bob_score_on_bob_node, DEFAULT_SCORE + 20); // Same

    // Both nodes converge on the same scores
    assert_eq!(alice_score_on_alice_node, alice_score_on_bob_node);
    assert_eq!(bob_score_on_alice_node, bob_score_on_bob_node);
}

/// Emergency proposal REJECTED → creator (false accuser) penalized on all nodes.
#[test]
fn test_scoring_pipeline_emergency_rejected() {
    let group_name = "scoring-rejected";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let bob_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let alice_mls = setup_mls(alice_hex);
    let mut alice_handle =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    let alice_id = alice_mls.wallet_bytes();

    let bob_mls = setup_mls(bob_hex);
    let mut bob_handle = prepare_to_join(
        group_name,
        bob_mls.wallet_bytes(),
        ProtocolConfig::new(1, 5).unwrap(),
    );
    let bob_kp = build_key_package_message(&bob_handle, &bob_mls, b"test-app-id").unwrap();
    let bob_id = bob_mls.wallet_bytes();

    let (welcome, _batch) = steward_add_joiner(&alice_mls, &mut alice_handle, &bob_kp);
    process_inbound(
        &mut bob_handle,
        &welcome.payload,
        WELCOME_SUBTOPIC,
        &bob_mls,
    )
    .unwrap();

    // Set up scoring on both nodes
    let mut alice_scoring = make_scoring();
    sync_scoring_members(&mut alice_scoring, group_name, &alice_handle, &alice_mls);

    let mut bob_scoring = make_scoring();
    sync_scoring_members(&mut bob_scoring, group_name, &bob_handle, &bob_mls);

    // Bob creates a false accusation against Alice
    let emergency = ViolationEvidence::censorship_inactivity(alice_id.clone(), 0)
        .with_creator(bob_id.clone())
        .into_update_request()
        .unwrap();
    let payload = emergency.encode_to_vec();

    let proposal_id = 60;
    bob_handle.store_voting_proposal(proposal_id, emergency);

    // Consensus rejects it (false accusation)

    // Bob's node: rejected owner (payload available from consensus service)
    apply_consensus_result(&mut bob_handle, proposal_id, false, &payload).unwrap();
    let bob_score_ops = emergency_score_ops(&payload, false);
    assert_eq!(bob_score_ops.len(), 1);
    assert_eq!(bob_score_ops[0].event, ScoreEvent::EmergencyNoCreator);
    assert_eq!(bob_score_ops[0].member_id, bob_id);
    bob_scoring.apply_ops(group_name, &bob_score_ops);

    // Alice's node: rejected non-owner
    apply_consensus_result(&mut alice_handle, proposal_id, false, &payload).unwrap();
    let alice_score_ops = emergency_score_ops(&payload, false);
    assert_eq!(alice_score_ops.len(), 1);
    assert_eq!(alice_score_ops[0].event, ScoreEvent::EmergencyNoCreator);
    assert_eq!(alice_score_ops[0].member_id, bob_id);
    alice_scoring.apply_ops(group_name, &alice_score_ops);

    // Both nodes: Alice untouched, Bob penalized
    assert_eq!(
        alice_scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE)
    );
    assert_eq!(
        alice_scoring.score_for(group_name, &bob_id),
        Some(DEFAULT_SCORE - 50)
    );
    assert_eq!(
        bob_scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE)
    );
    assert_eq!(
        bob_scoring.score_for(group_name, &bob_id),
        Some(DEFAULT_SCORE - 50)
    );
}

/// Non-emergency proposal produces no score ops.
#[test]
fn test_scoring_no_ops_for_regular_proposal() {
    let group_name = "scoring-regular";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    let alice_mls = setup_mls(alice_hex);
    let mut alice_handle =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();

    // Simulate a regular (non-emergency) proposal being approved as owner
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

    // Proposal should be in approved queue (ready for commit)
    assert_eq!(alice_handle.approved_proposals_count(), 1);
}

/// Core-level behavior: `sync_scoring_members` (which uses MLS members as
/// the source of truth) cannot reconstruct a joiner's prior scores — that
/// data lives in-memory on other nodes. The app/gateway layer closes this
/// gap via the `GroupSync` app message (see
/// `async_scoring_removal::test_ecp_scores_applied_on_all_nodes`). This
/// test pins the core-level contract: after a sync, new joiners see
/// members at the default score.
#[test]
fn test_new_joiner_starts_with_default_scores() {
    let group_name = "scoring-join-gap";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let bob_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let alice_mls = setup_mls(alice_hex);
    let mut alice_handle =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    let alice_id = alice_mls.wallet_bytes();

    // Alice's scoring has her at a non-default score (simulating prior events)
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

    // Bob joins
    let bob_mls = setup_mls(bob_hex);
    let mut bob_handle = prepare_to_join(
        group_name,
        bob_mls.wallet_bytes(),
        ProtocolConfig::new(1, 5).unwrap(),
    );
    let bob_kp = build_key_package_message(&bob_handle, &bob_mls, b"test-app-id").unwrap();

    let (welcome, _batch) = steward_add_joiner(&alice_mls, &mut alice_handle, &bob_kp);
    process_inbound(
        &mut bob_handle,
        &welcome.payload,
        WELCOME_SUBTOPIC,
        &bob_mls,
    )
    .unwrap();

    // Bob creates his scoring from MLS members — gets defaults
    let mut bob_scoring = make_scoring();
    sync_scoring_members(&mut bob_scoring, group_name, &bob_handle, &bob_mls);

    // At the core level, Bob sees Alice at the default score — real
    // scores arrive via the User-layer `GroupSync` message.
    assert_eq!(
        bob_scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE),
    );
}

/// Different violation types produce different target penalties.
/// Verifies the ViolationType → ScoreEvent mapping through apply_consensus_result.
#[test]
fn test_violation_type_specific_penalties() {
    let group_name = "scoring-violation-types";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let alice_mls = setup_mls(alice_hex);

    let creator_id = b"creator".to_vec();
    let target_id = vec![0xAA];

    // BrokenCommit → -50
    let mut group =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    let ecp = ViolationEvidence::broken_commit(target_id.clone(), 0, Vec::<u8>::new())
        .with_creator(creator_id.clone())
        .into_update_request()
        .unwrap();
    let payload = ecp.encode_to_vec();
    apply_consensus_result(&mut group, 1, true, &payload).unwrap();
    let score_ops = emergency_score_ops(&payload, true);
    assert_eq!(score_ops[0].event, ScoreEvent::BrokenCommit);

    let mut scoring = make_scoring();
    scoring.add_member(group_name, &target_id);
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: target_id.clone(),
            event: score_ops[0].event,
        },
    );
    assert_eq!(
        scoring.score_for(group_name, &target_id),
        Some(DEFAULT_SCORE - 50)
    );

    // BrokenMlsProposal → -30
    scoring.remove_member(group_name, &target_id);
    scoring.add_member(group_name, &target_id);
    let ecp = ViolationEvidence::broken_mls_proposal(target_id.clone(), 0, Vec::<u8>::new())
        .with_creator(creator_id.clone())
        .into_update_request()
        .unwrap();
    let payload = ecp.encode_to_vec();
    apply_consensus_result(&mut group, 2, true, &payload).unwrap();
    let score_ops = emergency_score_ops(&payload, true);
    assert_eq!(score_ops[0].event, ScoreEvent::BrokenMlsProposal);
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: target_id.clone(),
            event: score_ops[0].event,
        },
    );
    assert_eq!(
        scoring.score_for(group_name, &target_id),
        Some(DEFAULT_SCORE - 30)
    );

    // CensorshipInactivity → -40
    scoring.remove_member(group_name, &target_id);
    scoring.add_member(group_name, &target_id);
    let ecp = ViolationEvidence::censorship_inactivity(target_id.clone(), 0)
        .with_creator(creator_id.clone())
        .into_update_request()
        .unwrap();
    let payload = ecp.encode_to_vec();
    apply_consensus_result(&mut group, 3, true, &payload).unwrap();
    let score_ops = emergency_score_ops(&payload, true);
    assert_eq!(score_ops[0].event, ScoreEvent::CensorshipInactivity);
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: target_id.clone(),
            event: score_ops[0].event,
        },
    );
    assert_eq!(
        scoring.score_for(group_name, &target_id),
        Some(DEFAULT_SCORE - 40)
    );
}
