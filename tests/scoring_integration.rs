//! Integration test for the peer scoring pipeline.
//!
//! Verifies the full flow: group creation → member join → scoring init from MLS
//! members → emergency proposal → score ops applied → scores updated.
//!
//! This test operates at the core + scoring service level, without the async
//! User layer, to keep the test focused and synchronous.

use std::collections::HashMap;

use prost::Message;

use de_mls::app::{FixedScoringProvider, InMemoryPeerScoreStorage, PeerScoringService};
use de_mls::core::{
    FreezeFinalizeResult, Group, ProcessResult, ProtocolConfig, ScoreEvent, ScoringConfig,
    apply_consensus_result, build_key_package_message, create_commit_candidate, create_group,
    finalize_freeze_round, group_members, prepare_to_join, process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService, parse_wallet_address};
use de_mls::protos::de_mls::messages::v1::ViolationEvidence;

// ─────────────────────────── Helpers ───────────────────────────

const DEFAULT_SCORE: i64 = 100;
const REMOVAL_THRESHOLD: i64 = 0;

fn setup_mls(wallet_hex: &str) -> MlsService<MemoryDeMlsStorage> {
    let storage = MemoryDeMlsStorage::new();
    let mls = MlsService::new(storage);
    let wallet = parse_wallet_address(wallet_hex).unwrap();
    mls.init(wallet).unwrap();
    mls
}

fn default_scoring_service() -> PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider> {
    let deltas = HashMap::from([
        (ScoreEvent::SuccessfulCommit, 10),
        (ScoreEvent::BrokenCommit, -50),
        (ScoreEvent::BrokenMlsProposal, -30),
        (ScoreEvent::CensorshipInactivity, -40),
        (ScoreEvent::EmergencyYesCreator, 20),
        (ScoreEvent::EmergencyNoCreator, -50),
        (ScoreEvent::NonFinalizedProposalCommit, -30),
    ]);
    PeerScoringService::new(
        InMemoryPeerScoreStorage::new(),
        FixedScoringProvider::new(deltas),
        ScoringConfig {
            default_score: DEFAULT_SCORE,
            removal_threshold: REMOVAL_THRESHOLD,
        },
    )
}

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

/// Steward processes joiner's key package and creates a commit that adds them.
/// Returns (welcome_packet, batch_packet).
fn steward_add_joiner(
    steward_mls: &MlsService<MemoryDeMlsStorage>,
    steward_handle: &mut Group,
    joiner_kp_packet: &OutboundPacket,
) -> (OutboundPacket, OutboundPacket) {
    let result = process_inbound(
        steward_handle,
        &joiner_kp_packet.payload,
        WELCOME_SUBTOPIC,
        steward_mls,
    )
    .unwrap();

    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    steward_handle.insert_approved_proposal(1, gur);
    let packets = create_commit_candidate(steward_handle, steward_mls, b"test-app-id").unwrap();

    let finalize =
        finalize_freeze_round(steward_handle, steward_mls, false, b"test-app-id").unwrap();
    let welcome_packet = match finalize {
        FreezeFinalizeResult::Applied { result, outbound } => {
            assert!(
                matches!(result, ProcessResult::GroupUpdated),
                "Expected GroupUpdated, got {:?}",
                result
            );
            outbound
                .into_iter()
                .find(|p| p.subtopic == WELCOME_SUBTOPIC)
                .expect("Expected welcome packet")
        }
        other => panic!("Expected Applied, got {:?}", other),
    };

    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch packet")
        .clone();

    (welcome_packet, batch_packet)
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

    let mut scoring = default_scoring_service();

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
    let mut bob_scoring = default_scoring_service();
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
    let bob_result = apply_consensus_result(&mut bob_handle, proposal_id, true, &payload).unwrap();
    assert_eq!(bob_result.score_ops.len(), 2);
    for op in &bob_result.score_ops {
        bob_scoring.apply_event(group_name, &op.member_id, op.event);
    }

    // Alice's node: approved non-owner (she receives the payload)
    let alice_result =
        apply_consensus_result(&mut alice_handle, proposal_id, true, &payload).unwrap();
    assert_eq!(alice_result.score_ops.len(), 2);
    for op in &alice_result.score_ops {
        scoring.apply_event(group_name, &op.member_id, op.event);
    }

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
    let mut alice_scoring = default_scoring_service();
    sync_scoring_members(&mut alice_scoring, group_name, &alice_handle, &alice_mls);

    let mut bob_scoring = default_scoring_service();
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
    let bob_result = apply_consensus_result(&mut bob_handle, proposal_id, false, &payload).unwrap();
    assert_eq!(bob_result.score_ops.len(), 1);
    assert_eq!(
        bob_result.score_ops[0].event,
        ScoreEvent::EmergencyNoCreator
    );
    assert_eq!(bob_result.score_ops[0].member_id, bob_id);
    for op in &bob_result.score_ops {
        bob_scoring.apply_event(group_name, &op.member_id, op.event);
    }

    // Alice's node: rejected non-owner
    let alice_result =
        apply_consensus_result(&mut alice_handle, proposal_id, false, &payload).unwrap();
    assert_eq!(alice_result.score_ops.len(), 1);
    assert_eq!(
        alice_result.score_ops[0].event,
        ScoreEvent::EmergencyNoCreator
    );
    assert_eq!(alice_result.score_ops[0].member_id, bob_id);
    for op in &alice_result.score_ops {
        alice_scoring.apply_event(group_name, &op.member_id, op.event);
    }

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

    let result = apply_consensus_result(&mut alice_handle, proposal_id, true, &payload).unwrap();
    assert!(result.score_ops.is_empty());

    // Proposal should be in approved queue (ready for commit)
    assert_eq!(alice_handle.approved_proposals_count(), 1);
}

/// Known gap: new joiners start with default scores, not the group's actual scores.
/// This test documents the gap — once score sync messages are implemented (M4),
/// this test should be updated to verify synced scores.
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
    let mut alice_scoring = default_scoring_service();
    alice_scoring.add_member(group_name, &alice_id);
    alice_scoring.apply_event(group_name, &alice_id, ScoreEvent::EmergencyYesCreator);
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
    let mut bob_scoring = default_scoring_service();
    sync_scoring_members(&mut bob_scoring, group_name, &bob_handle, &bob_mls);

    // GAP: Bob sees Alice at default, not her actual score (120)
    assert_eq!(
        bob_scoring.score_for(group_name, &alice_id),
        Some(DEFAULT_SCORE), // Should be 120 once score sync is implemented
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
    let result = apply_consensus_result(&mut group, 1, true, &payload).unwrap();
    assert_eq!(result.score_ops[0].event, ScoreEvent::BrokenCommit);

    let mut scoring = default_scoring_service();
    scoring.add_member(group_name, &target_id);
    scoring.apply_event(group_name, &target_id, result.score_ops[0].event);
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
    let result = apply_consensus_result(&mut group, 2, true, &payload).unwrap();
    assert_eq!(result.score_ops[0].event, ScoreEvent::BrokenMlsProposal);
    scoring.apply_event(group_name, &target_id, result.score_ops[0].event);
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
    let result = apply_consensus_result(&mut group, 3, true, &payload).unwrap();
    assert_eq!(result.score_ops[0].event, ScoreEvent::CensorshipInactivity);
    scoring.apply_event(group_name, &target_id, result.score_ops[0].event);
    assert_eq!(
        scoring.score_for(group_name, &target_id),
        Some(DEFAULT_SCORE - 40)
    );
}
