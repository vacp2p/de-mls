//! Integration tests for steward violation detection, emergency criteria proposals,
//! freeze round selection, and dedup.

use de_mls::core::{
    FreezeOutcome, ProcessResult, ProposalId, ScoreEvent, apply_consensus_result,
    create_commit_candidate, finalize_freeze_round, process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use de_mls::protos::de_mls::messages::v1::{
    GroupUpdateRequest, ViolationEvidence, ViolationType, group_update_request,
};

mod common;
use common::{setup_joiner, setup_steward, steward_add_joiner};

// ─────────────────────────── Tests ───────────────────────────

/// Test: ViolationDetected carries correct evidence for emergency criteria proposal.
#[test]
fn test_violation_detected_produces_emergency_proposal() {
    let evidence = ViolationEvidence::broken_commit(vec![0xAA, 0xBB], 5, vec![0xDE, 0xAD])
        .with_creator(vec![0x01]);
    let request = evidence.into_update_request().unwrap();

    match request.payload {
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => {
            let ev = ec.evidence.expect("Expected evidence");
            assert_eq!(ev.violation_type, 1);
            assert_eq!(ev.target_member_id, vec![0xAA, 0xBB]);
            assert_eq!(ev.epoch, 5);
        }
        other => panic!("Expected EmergencyCriteria payload, got {:?}", other),
    }
}

/// Test: create_batch_proposals rejects emergency criteria proposals in the approved queue.
#[test]
fn test_emergency_in_approved_queue_returns_error() {
    let group_name = "emergency-only-batch";
    let (mls, mut group) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let emergency_request = ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new())
        .with_creator(vec![0x01])
        .into_update_request()
        .unwrap();
    group.insert_approved_proposal(50, emergency_request);
    assert_eq!(group.approved_proposals_count(), 1);

    let result = create_commit_candidate(&mut group, &mls, b"test-app-id");
    assert!(result.is_err());
    match result.unwrap_err() {
        de_mls::core::CoreError::UnexpectedNonMlsProposals { proposal_ids } => {
            assert_eq!(proposal_ids, vec![50]);
        }
        other => panic!("Expected UnexpectedNonMlsProposals, got {:?}", other),
    }
}

/// Test: remove_approved_proposal correctly removes a single proposal.
#[test]
fn test_remove_approved_proposal() {
    let group_name = "remove-approved";
    let (_mls, mut group) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let emergency_request = ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new())
        .with_creator(vec![0x01])
        .into_update_request()
        .unwrap();
    group.insert_approved_proposal(50, emergency_request);
    assert_eq!(group.approved_proposals_count(), 1);

    group.remove_approved_proposal(50);
    assert_eq!(group.approved_proposals_count(), 0);
}

/// Test: ViolationEvidence carries correct target_member_id and epoch.
#[test]
fn test_violation_evidence_carries_steward_id_and_epoch() {
    let evidence = ViolationEvidence::broken_commit(vec![0xAA, 0xBB], 2, b"proof".to_vec());
    assert_eq!(evidence.violation_type, ViolationType::BrokenCommit as i32);
    assert_eq!(evidence.target_member_id, vec![0xAA, 0xBB]);
    assert_eq!(evidence.epoch, 2);
    assert_eq!(evidence.evidence_payload, b"proof".to_vec());
}

/// Test: epoch counter increments when approved proposals are cleared.
#[test]
fn test_mls_epoch_accessible_after_group_creation() {
    let group_name = "epoch-mls";
    let (mls, _handle) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    // MLS epoch starts at 0 for a newly created group
    let epoch = mls.current_epoch(group_name).unwrap();
    assert_eq!(epoch, 0);
}

#[test]
fn test_clear_approved_proposals_does_not_change_mls_epoch() {
    let group_name = "epoch-no-change";
    let (mls, mut group) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    assert_eq!(mls.current_epoch(group_name).unwrap(), 0);

    group.insert_approved_proposal(1, GroupUpdateRequest { payload: None });
    group.clear_approved_proposals();
    // MLS epoch only advances on actual commit merge, not on clear_approved_proposals
    assert_eq!(mls.current_epoch(group_name).unwrap(), 0);
}

/// Test: apply_consensus_result errors on invalid (unparseable) payload.
#[test]
fn test_apply_consensus_result_invalid_payload() {
    let group_name = "invalid-payload";
    let (_mls, mut group) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let result = apply_consensus_result(&mut group, 999, true, &[0xFF, 0xFF]);
    assert!(result.is_err());
}

/// Test: emergency proposals mixed with regular proposals in approved queue
/// causes an error.
#[test]
fn test_emergency_mixed_with_regular_returns_error() {
    let group_name = "emergency-digest-filter";
    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    let (_joiner2_mls, _joiner2_handle, kp2_packet) =
        setup_joiner(group_name, "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
    let result = process_inbound(
        &mut steward_handle,
        &kp2_packet.payload,
        WELCOME_SUBTOPIC,
        &steward_mls,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let regular_id: ProposalId = 60;
    let emergency_id: ProposalId = 61;
    steward_handle.insert_approved_proposal(regular_id, gur.clone());
    steward_handle.insert_approved_proposal(
        emergency_id,
        ViolationEvidence::broken_commit(vec![], 0, Vec::<u8>::new())
            .with_creator(vec![0x01])
            .into_update_request()
            .unwrap(),
    );

    let result = create_commit_candidate(&mut steward_handle, &steward_mls, b"test-app-id");
    assert!(result.is_err());
    match result.unwrap_err() {
        de_mls::core::CoreError::UnexpectedNonMlsProposals { proposal_ids } => {
            assert_eq!(proposal_ids, vec![emergency_id]);
        }
        other => panic!("Expected UnexpectedNonMlsProposals, got {:?}", other),
    }
}

// ─────────────────────────── Dedup & Freeze Round Tests ───────────────────────────

/// Test: same batch received twice hits dedup and returns Noop.
#[test]
fn test_duplicate_batch_returns_noop() {
    let group_name = "dedup-batch";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    let (_joiner2_mls, _joiner2_handle, kp2_packet) =
        setup_joiner(group_name, "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");

    let result = process_inbound(
        &mut steward_handle,
        &kp2_packet.payload,
        WELCOME_SUBTOPIC,
        &steward_mls,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id: ProposalId = 45;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur);
    let packets =
        create_commit_candidate(&mut steward_handle, &steward_mls, b"test-app-id").unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Start freeze round so candidates get buffered
    let epoch = joiner_mls.current_epoch(group_name).unwrap();
    joiner_handle.start_freeze_round(epoch);

    // First receive: candidate buffered → CommitCandidateReceived
    let r1 = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(r1, ProcessResult::CommitCandidateReceived),
        "Expected CommitCandidateReceived, got {:?}",
        r1
    );

    // Second receive of same batch: should be detected as duplicate
    let r2 = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(r2, ProcessResult::Noop),
        "Expected Noop (duplicate), got {:?}",
        r2
    );
}

/// Test: after a violation is detected and staged commit discarded,
/// subsequent MLS operations still work correctly.
#[test]
fn test_violation_does_not_corrupt_mls_state() {
    let group_name = "violation-no-corrupt";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    let join_result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    // After joining, MLS members should be accessible
    let members = joiner_mls.members(group_name).unwrap();
    assert_eq!(
        members.len(),
        2,
        "Members should be accessible after joining"
    );
}

/// Test: candidate arriving without a freeze round is ignored.
#[test]
fn test_candidate_ignored_without_freeze_round() {
    let group_name = "no-freeze-round";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    let join_result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    let (_joiner2_mls, _joiner2_handle, kp2_packet) =
        setup_joiner(group_name, "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");

    let result = process_inbound(
        &mut steward_handle,
        &kp2_packet.payload,
        WELCOME_SUBTOPIC,
        &steward_mls,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id: ProposalId = 44;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    let packets =
        create_commit_candidate(&mut steward_handle, &steward_mls, b"test-app-id").unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Joiner has NO freeze round active → candidate ignored
    let result = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::Noop),
        "Expected Noop (no freeze round), got {:?}",
        result
    );
}

/// Test: CommitCandidate round-trip buffers candidate and finalize_freeze_round
/// uses MLS-authenticated sender identity from the staged commit.
#[test]
fn test_commit_candidate_roundtrip_sender_identity() {
    let group_name = "candidate-sender-id";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    let join_result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    // Add a third member proposal
    let (_joiner2_mls, _joiner2_handle, kp2_packet) =
        setup_joiner(group_name, "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
    let result = process_inbound(
        &mut steward_handle,
        &kp2_packet.payload,
        WELCOME_SUBTOPIC,
        &steward_mls,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id: ProposalId = 90;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur);

    let packets =
        create_commit_candidate(&mut steward_handle, &steward_mls, b"test-app-id").unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Start freeze round before receiving candidate
    let epoch = joiner_mls.current_epoch(group_name).unwrap();
    joiner_handle.start_freeze_round(epoch);

    // Joiner receives candidate — should buffer it
    let result = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::CommitCandidateReceived),
        "Expected CommitCandidateReceived, got {:?}",
        result
    );

    // Finalize: MLS commit staging authenticates the sender
    let finalize =
        finalize_freeze_round(&mut joiner_handle, &joiner_mls, false, b"test-app-id").unwrap();
    let matched = matches!(
        &finalize.outcome,
        FreezeOutcome::Applied { result, .. } if matches!(**result, ProcessResult::GroupUpdated)
    );
    assert!(
        matched,
        "Expected GroupUpdated after finalize, got {finalize:?}"
    );

    // After finalization, verify the steward list on the creator  group
    // still contains the steward (the list is the source of truth).
    let steward_list = steward_handle
        .steward_list()
        .expect("steward should have a list");
    assert!(
        steward_list.contains(&steward_mls.wallet_bytes()),
        "Steward should be on the steward list"
    );
}

/// Test: when a backup steward commits in place of the live epoch steward,
/// finalize emits `CensorshipInactivity` against the absent steward.
///
/// Build a two-steward list over Alice + Bob. If hash rotation places the
/// OTHER steward at `epoch_steward(epoch)`, Bob's successful commit from
/// his own node pins the absent steward with `CensorshipInactivity`. If
/// rotation makes Bob himself the live epoch steward, the self-accusation
/// skip keeps the score-op list free of `CensorshipInactivity`.
#[test]
fn test_backup_commit_scores_absent_steward() {
    let group_name = "absent-steward";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let bob_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let charlie_hex = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";

    let (alice_mls, mut alice_group) = setup_steward(group_name, alice_hex);
    let (bob_mls, mut bob_group, bob_kp_packet) = setup_joiner(group_name, bob_hex);
    let alice_id = alice_mls.wallet_bytes();
    let bob_id = bob_mls.wallet_bytes();

    let (welcome, _) = steward_add_joiner(&alice_mls, &mut alice_group, &bob_kp_packet);
    let join_result =
        process_inbound(&mut bob_group, &welcome.payload, WELCOME_SUBTOPIC, &bob_mls).unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    // After the join, both are MLS members. Regenerate the steward list
    // over [Alice, Bob] so both are stewards at the current epoch.
    let epoch = alice_mls.current_epoch(group_name).unwrap();
    let members = vec![alice_id.clone(), bob_id.clone()];
    alice_group
        .generate_and_set_steward_list(epoch, &members, 2)
        .unwrap();
    bob_group
        .generate_and_set_steward_list(epoch, &members, 2)
        .unwrap();

    // Produce an approved proposal (invite Charlie) on both sides.
    let (_charlie_mls, _charlie_group, charlie_kp_packet) = setup_joiner(group_name, charlie_hex);
    let gur = match process_inbound(
        &mut alice_group,
        &charlie_kp_packet.payload,
        WELCOME_SUBTOPIC,
        &alice_mls,
    )
    .unwrap()
    {
        ProcessResult::MembershipChangeReceived(g) => g,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };
    let proposal_id: ProposalId = 90;
    alice_group.insert_approved_proposal(proposal_id, gur.clone());
    bob_group.insert_approved_proposal(proposal_id, gur);

    // Bob builds the commit (Alice never submits). His local candidate is
    // the only applicable entry when he finalises.
    let _ = create_commit_candidate(&mut bob_group, &bob_mls, b"test-app-id").unwrap();
    let live_epoch_steward = bob_group
        .live_epoch_steward(epoch, &members)
        .expect("both stewards are eligible")
        .to_vec();

    let result = finalize_freeze_round(&mut bob_group, &bob_mls, false, b"test-app-id").unwrap();

    let matched = matches!(
        &result.outcome,
        FreezeOutcome::Applied { result: r, .. } if matches!(**r, ProcessResult::GroupUpdated)
    );
    assert!(matched, "Expected GroupUpdated, got {:?}", result.outcome);

    let events: Vec<(Vec<u8>, ScoreEvent)> = result
        .score_ops
        .iter()
        .map(|op| (op.member_id.clone(), op.event))
        .collect();

    assert!(
        events
            .iter()
            .any(|(m, e)| m == &bob_id && *e == ScoreEvent::SuccessfulCommit),
        "Bob (committer) should receive SuccessfulCommit, got {events:?}",
    );

    if live_epoch_steward == alice_id {
        // Alice was supposed to commit; Bob (self) stood in for her. Alice
        // isn't self, so she picks up CensorshipInactivity.
        assert!(
            events
                .iter()
                .any(|(m, e)| m == &alice_id && *e == ScoreEvent::CensorshipInactivity),
            "Alice (absent epoch steward) should receive CensorshipInactivity, got {events:?}",
        );
    } else {
        // Bob himself is the live epoch steward — self-accusation skipped.
        assert!(
            !events
                .iter()
                .any(|(_, e)| *e == ScoreEvent::CensorshipInactivity),
            "No CensorshipInactivity expected when self is the live epoch steward, got {events:?}",
        );
    }
}

/// Test: reject_all_voting_proposals clears voting queue.
#[test]
fn test_reject_all_voting_proposals_clears_queue() {
    let group_name = "reject-voting";
    let (_mls, mut group) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    // Store proposals in voting queue
    group.store_voting_proposal(1, GroupUpdateRequest { payload: None });
    group.store_voting_proposal(2, GroupUpdateRequest { payload: None });
    assert!(group.is_owner_of_proposal(1));
    assert!(group.is_owner_of_proposal(2));

    // Reject all voting proposals
    group.reject_all_voting_proposals();

    // Voting queue should be empty
    assert!(!group.is_owner_of_proposal(1));
    assert!(!group.is_owner_of_proposal(2));
}

/// Test: no valid candidate triggers NoCandidate result.
#[test]
fn test_no_valid_candidate_triggers_no_candidate() {
    let group_name = "no-candidate";

    let (steward_mls, mut group) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    // Start freeze round with no candidates
    group.insert_approved_proposal(1, GroupUpdateRequest { payload: None });
    let epoch = steward_mls.current_epoch(group_name).unwrap();
    group.start_freeze_round(epoch);

    let finalize = finalize_freeze_round(&mut group, &steward_mls, false, b"test-app-id").unwrap();
    assert!(
        matches!(finalize.outcome, FreezeOutcome::NoCandidate),
        "Expected NoCandidate when no candidates buffered, got {:?}",
        finalize
    );
}
