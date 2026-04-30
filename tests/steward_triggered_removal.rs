//! Steward-triggered removal pipeline.
//!
//! Verifies the flow: score drops below threshold → steward creates
//! SCORE_BELOW_THRESHOLD ECP → consensus vote → YES transforms into
//! RemoveMember in approved queue (or NO penalizes creator).

use prost::Message;

use de_mls::app::emergency_score_ops;
use de_mls::core::{ProtocolConfig, ScoreEvent, ScoreOp, apply_consensus_result, create_group};
use de_mls::protos::de_mls::messages::v1::{
    ViolationEvidence, ViolationType, group_update_request,
};

mod common;
use common::{make_scoring, setup_mls};

// ─────────────────────────── Tests ───────────────────────────

/// ECP YES for SCORE_BELOW_THRESHOLD: RemoveMember appears in approved queue + correct score ops.
#[test]
fn test_score_below_threshold_yes_transforms_to_remove_member() {
    let group_name = "removal-yes";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let alice_mls = setup_mls(alice_hex);
    let mut group =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    let steward_id = alice_mls.wallet_bytes().to_vec();

    let target_id = vec![0xBB];
    let proposal_id = 100;

    // Steward creates SCORE_BELOW_THRESHOLD ECP
    let evidence = ViolationEvidence::score_below_threshold(target_id.clone(), 0, -10)
        .with_creator(steward_id.clone());
    let request = evidence.into_update_request().unwrap();
    let payload = request.encode_to_vec();

    // Owner path: store in voting queue first
    group.store_voting_proposal(proposal_id, request);

    // Consensus approves
    apply_consensus_result(&mut group, proposal_id, true, &payload).unwrap();
    let score_ops = emergency_score_ops(&payload, true);

    // Score ops: creator rewarded only (target already at threshold, penalty skipped)
    assert_eq!(score_ops.len(), 1);
    assert_eq!(score_ops[0].member_id, steward_id);
    assert_eq!(score_ops[0].event, ScoreEvent::EmergencyYesCreator);

    // RemoveMember should be in the approved queue (transformed from ECP)
    assert_eq!(group.approved_proposals_count(), 1);
    let approved = group.approved_proposals();
    let (_, gur) = approved.iter().next().unwrap();
    match &gur.payload {
        Some(group_update_request::Payload::RemoveMember(rm)) => {
            assert_eq!(rm.identity, target_id);
        }
        other => panic!("Expected RemoveMember, got {:?}", other),
    }
}

/// ECP YES for SCORE_BELOW_THRESHOLD (non-owner path): RemoveMember in approved queue.
#[test]
fn test_score_below_threshold_yes_non_owner() {
    let group_name = "removal-yes-nonowner";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let alice_mls = setup_mls(alice_hex);
    let mut group =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();

    let target_id = vec![0xBB];
    let creator_id = vec![0xCC]; // someone else
    let proposal_id = 101;

    let evidence = ViolationEvidence::score_below_threshold(target_id.clone(), 0, -5)
        .with_creator(creator_id.clone());
    let request = evidence.into_update_request().unwrap();
    let payload = request.encode_to_vec();

    // Non-owner: do NOT store in voting queue
    apply_consensus_result(&mut group, proposal_id, true, &payload).unwrap();
    let score_ops = emergency_score_ops(&payload, true);

    // Score ops: creator rewarded only (target penalty skipped)
    assert_eq!(score_ops.len(), 1);
    assert_eq!(score_ops[0].event, ScoreEvent::EmergencyYesCreator);

    // RemoveMember should be in approved queue
    assert_eq!(group.approved_proposals_count(), 1);
    let approved = group.approved_proposals();
    let (_, gur) = approved.iter().next().unwrap();
    match &gur.payload {
        Some(group_update_request::Payload::RemoveMember(rm)) => {
            assert_eq!(rm.identity, target_id);
        }
        other => panic!("Expected RemoveMember, got {:?}", other),
    }
}

/// ECP NO for SCORE_BELOW_THRESHOLD: no RemoveMember, creator penalized.
#[test]
fn test_score_below_threshold_no_penalizes_creator() {
    let group_name = "removal-no";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let alice_mls = setup_mls(alice_hex);
    let mut group =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    let steward_id = alice_mls.wallet_bytes().to_vec();

    let target_id = vec![0xBB];
    let proposal_id = 102;

    let evidence = ViolationEvidence::score_below_threshold(target_id.clone(), 0, -10)
        .with_creator(steward_id.clone());
    let request = evidence.into_update_request().unwrap();
    let payload = request.encode_to_vec();

    group.store_voting_proposal(proposal_id, request);

    // Consensus rejects
    apply_consensus_result(&mut group, proposal_id, false, &payload).unwrap();
    let score_ops = emergency_score_ops(&payload, false);

    // Creator penalized, target unaffected
    assert_eq!(score_ops.len(), 1);
    assert_eq!(score_ops[0].member_id, steward_id);
    assert_eq!(score_ops[0].event, ScoreEvent::EmergencyNoCreator);

    // No RemoveMember in approved queue
    assert_eq!(group.approved_proposals_count(), 0);
}

/// Full pipeline: penalties → below threshold → ECP → YES → RemoveMember in queue.
#[test]
fn test_full_pipeline_penalties_to_removal() {
    let group_name = "removal-pipeline";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let alice_mls = setup_mls(alice_hex);
    let mut group =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    let steward_id = alice_mls.wallet_bytes().to_vec();
    let target_id = vec![0xDD];

    let threshold = group.threshold_peer_score();
    let mut scoring = make_scoring();
    scoring.add_member(group_name, &steward_id);
    scoring.add_member(group_name, &target_id);

    // Apply penalties until target drops below threshold
    // 100 - 50 = 50 (BrokenCommit)
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: target_id.clone(),
            event: ScoreEvent::BrokenCommit,
        },
    );
    assert!(!scoring.is_below_threshold(group_name, &target_id, threshold));

    // 50 - 50 = 0 (EmergencyNoCreator)
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: target_id.clone(),
            event: ScoreEvent::EmergencyNoCreator,
        },
    );
    assert!(scoring.is_below_threshold(group_name, &target_id, threshold));

    // Now steward creates SCORE_BELOW_THRESHOLD ECP
    let below = scoring.members_below_threshold(group_name, threshold);
    assert!(below.contains(&target_id));

    let current_score = scoring.score_for(group_name, &target_id).unwrap();
    let evidence = ViolationEvidence::score_below_threshold(target_id.clone(), 0, current_score)
        .with_creator(steward_id.clone());
    let request = evidence.into_update_request().unwrap();
    let payload = request.encode_to_vec();

    let proposal_id = 200;
    group.store_voting_proposal(proposal_id, request);

    // Consensus approves
    apply_consensus_result(&mut group, proposal_id, true, &payload).unwrap();
    let score_ops = emergency_score_ops(&payload, true);

    // Apply score ops
    scoring.apply_ops(group_name, &score_ops);

    // RemoveMember should be in approved queue
    assert_eq!(group.approved_proposals_count(), 1);
    let approved = group.approved_proposals();
    let (_, gur) = approved.iter().next().unwrap();
    match &gur.payload {
        Some(group_update_request::Payload::RemoveMember(rm)) => {
            assert_eq!(rm.identity, target_id);
        }
        other => panic!("Expected RemoveMember, got {:?}", other),
    }
}

/// Dedup: Group prevents duplicate ECP for same target.
#[test]
fn test_dedup_pending_removal_targets() {
    let mut group = create_group(
        "dedup-group",
        &setup_mls("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
        ProtocolConfig::new(1, 5).unwrap(),
    )
    .unwrap();
    let target = vec![0xAA];

    assert!(!group.has_pending_removal(&target));

    group.observe_pending_removal(target.clone());
    assert!(group.has_pending_removal(&target));

    // Second observation is idempotent
    group.observe_pending_removal(target.clone());
    assert!(group.has_pending_removal(&target));

    // Resolve clears it
    group.resolve_pending_removal(&target);
    assert!(!group.has_pending_removal(&target));
}

/// Self-guard: steward skips itself when checking members below threshold.
#[test]
fn test_steward_skips_self_for_removal() {
    let group_name = "removal-self-guard";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let alice_mls = setup_mls(alice_hex);
    let group = create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    let threshold = group.threshold_peer_score();

    let mut scoring = make_scoring();

    let steward_id = vec![0x01];
    let other_id = vec![0x02];

    scoring.add_member(group_name, &steward_id);
    scoring.add_member(group_name, &other_id);

    // Drop both below threshold
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: steward_id.clone(),
            event: ScoreEvent::BrokenCommit,
        },
    );
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: steward_id.clone(),
            event: ScoreEvent::EmergencyNoCreator,
        },
    );
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: other_id.clone(),
            event: ScoreEvent::BrokenCommit,
        },
    );
    scoring.apply_op(
        group_name,
        &ScoreOp {
            member_id: other_id.clone(),
            event: ScoreEvent::EmergencyNoCreator,
        },
    );

    assert!(scoring.is_below_threshold(group_name, &steward_id, threshold));
    assert!(scoring.is_below_threshold(group_name, &other_id, threshold));

    // Filter as the steward would
    let targets: Vec<Vec<u8>> = scoring
        .members_below_threshold(group_name, threshold)
        .into_iter()
        .filter(|id| *id != steward_id)
        .collect();

    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0], other_id);
}

/// `members_below_threshold` selects against the threshold passed in,
/// not a global default.
#[test]
fn test_members_below_threshold_uses_per_group_value() {
    let group_name = "per-group-threshold";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let alice_mls = setup_mls(alice_hex);
    let group = create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();
    assert_eq!(
        group.threshold_peer_score(),
        de_mls::core::DEFAULT_THRESHOLD_PEER_SCORE
    );

    let alice = vec![0xAA];
    let bob = vec![0xBB];
    let carol = vec![0xCC];

    let mut scoring = make_scoring();
    scoring.add_member(group_name, &alice);
    scoring.add_member(group_name, &bob);
    scoring.add_member(group_name, &carol);
    scoring.set_score(group_name, &alice, -10);
    scoring.set_score(group_name, &bob, -30);
    scoring.set_score(group_name, &carol, -60);

    // Strict threshold (-50): only Carol qualifies for removal.
    let strict = scoring.members_below_threshold(group_name, -50);
    assert!(strict.contains(&carol));
    assert!(!strict.contains(&bob));
    assert!(!strict.contains(&alice));

    // Loose threshold (-25, e.g. after a GroupSync reduced strictness):
    // both Bob and Carol qualify; Alice still safe.
    let loose = scoring.members_below_threshold(group_name, -25);
    assert!(loose.contains(&bob));
    assert!(loose.contains(&carol));
    assert!(!loose.contains(&alice));
}

/// ViolationEvidence::score_below_threshold constructor correctness.
#[test]
fn test_violation_evidence_score_below_threshold_constructor() {
    let target = vec![0xAA, 0xBB];
    let epoch = 42;
    let score = -15_i64;

    let ev = ViolationEvidence::score_below_threshold(target.clone(), epoch, score);

    assert_eq!(ev.violation_type, ViolationType::ScoreBelowThreshold as i32);
    assert_eq!(ev.target_member_id, target);
    assert_eq!(ev.epoch, epoch);
    assert_eq!(ev.evidence_payload, score.to_le_bytes().to_vec());
    assert!(ev.creator_member_id.is_empty());

    // with_creator works
    let creator = vec![0xCC];
    let ev = ev.with_creator(creator.clone());
    assert_eq!(ev.creator_member_id, creator);

    // into_update_request works
    let gur = ev.into_update_request().unwrap();
    match &gur.payload {
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => {
            let inner_ev = ec.evidence.as_ref().unwrap();
            assert_eq!(
                inner_ev.violation_type,
                ViolationType::ScoreBelowThreshold as i32
            );
            assert_eq!(inner_ev.target_member_id, target);
        }
        other => panic!("Expected EmergencyCriteria, got {:?}", other),
    }
}

/// Regular (non-score) emergency YES still removes from approved queue (no transform).
#[test]
fn test_regular_emergency_yes_no_transform() {
    let group_name = "no-transform";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let alice_mls = setup_mls(alice_hex);
    let mut group =
        create_group(group_name, &alice_mls, ProtocolConfig::new(1, 5).unwrap()).unwrap();

    let target_id = vec![0xBB];
    let creator_id = alice_mls.wallet_bytes().to_vec();
    let proposal_id = 300;

    // Regular violation (not score-below-threshold)
    let evidence = ViolationEvidence::broken_commit(target_id.clone(), 0, Vec::<u8>::new())
        .with_creator(creator_id);
    let request = evidence.into_update_request().unwrap();
    let payload = request.encode_to_vec();

    group.store_voting_proposal(proposal_id, request);

    apply_consensus_result(&mut group, proposal_id, true, &payload).unwrap();
    let score_ops = emergency_score_ops(&payload, true);

    // Score ops present (it's an accepted emergency)
    assert_eq!(score_ops.len(), 2);

    // But NO RemoveMember in approved queue (regular emergencies are consumed)
    assert_eq!(group.approved_proposals_count(), 0);
}
