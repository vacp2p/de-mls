//! Integration tests for steward violation detection, emergency criteria
//! proposals, freeze round selection, and dedup.

use de_mls::core::StewardListPlugin;
use de_mls::core::{FreezeOutcome, ProcessResult, ProposalId, ScoreEvent, finalize_freeze_round};
use de_mls::ds::{APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use de_mls::member_id::MemberId;
use de_mls::mls_crypto::MlsService;
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, ConversationUpdateRequest, ViolationEvidence, app_message,
};
use prost::Message;

mod common;
use common::{
    build_commit_candidate, process_inbound_compat, setup_joiner, setup_steward, steward_add_joiner,
};

// ─────────────────────────── Tests ───────────────────────────

/// Test: emergency proposals mixed with regular proposals in approved queue
/// causes an error.
#[test]
fn test_emergency_mixed_with_regular_returns_error() {
    let conversation_id = "emergency-digest-filter";
    let mut steward_handle = setup_steward(
        conversation_id,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_id,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_bytes, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome(&welcome_bytes);

    let joiner2 = setup_joiner(
        conversation_id,
        "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    );
    let result = process_inbound_compat(
        &mut steward_handle.group,
        Some(&mut steward_handle.mls),
        &joiner2.kp_packet.payload,
        WELCOME_SUBTOPIC,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => *gur,
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

    let result = build_commit_candidate(
        &mut steward_handle.group,
        &mut steward_handle.mls,
        &steward_handle.steward_list,
        false,
        &steward_handle.member_id,
        b"test-app-id",
    );
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
    let conversation_id = "dedup-batch";

    let mut steward_handle = setup_steward(
        conversation_id,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_id,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_bytes, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome(&welcome_bytes);

    let joiner2 = setup_joiner(
        conversation_id,
        "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    );

    let result = process_inbound_compat(
        &mut steward_handle.group,
        Some(&mut steward_handle.mls),
        &joiner2.kp_packet.payload,
        WELCOME_SUBTOPIC,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => *gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id: ProposalId = 45;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner.group.insert_approved_proposal(proposal_id, gur);
    let packets = build_commit_candidate(
        &mut steward_handle.group,
        &mut steward_handle.mls,
        &steward_handle.steward_list,
        false,
        &steward_handle.member_id,
        b"test-app-id",
    )
    .unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Start freeze round so candidates get buffered
    let epoch = joiner.mls.as_ref().unwrap().current_epoch().unwrap();
    joiner.group.start_freeze_round(epoch);

    // First receive: candidate buffered → CommitCandidateReceived
    let r1 = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_mut(),
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    assert!(
        matches!(r1, ProcessResult::CommitCandidateReceived { .. }),
        "Expected CommitCandidateReceived, got {:?}",
        r1
    );

    // Second receive of same batch: should be detected as duplicate
    let r2 = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_mut(),
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    assert!(
        matches!(r2, ProcessResult::Noop(_)),
        "Expected Noop (duplicate), got {:?}",
        r2
    );
}

/// Test: candidate arriving without a freeze round is ignored.
#[test]
fn test_candidate_ignored_without_freeze_round() {
    let conversation_id = "no-freeze-round";

    let mut steward_handle = setup_steward(
        conversation_id,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_id,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_bytes, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome(&welcome_bytes);

    let joiner2 = setup_joiner(
        conversation_id,
        "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    );

    let result = process_inbound_compat(
        &mut steward_handle.group,
        Some(&mut steward_handle.mls),
        &joiner2.kp_packet.payload,
        WELCOME_SUBTOPIC,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => *gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id: ProposalId = 44;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    let packets = build_commit_candidate(
        &mut steward_handle.group,
        &mut steward_handle.mls,
        &steward_handle.steward_list,
        false,
        &steward_handle.member_id,
        b"test-app-id",
    )
    .unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Joiner has NO freeze round active → candidate ignored
    let result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_mut(),
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::Noop(_)),
        "Expected Noop (no freeze round), got {:?}",
        result
    );
}

/// Test: CommitCandidate round-trip buffers candidate and finalize_freeze_round
/// uses MLS-authenticated sender identity from the staged commit.
#[test]
fn test_commit_candidate_roundtrip_sender_identity() {
    let conversation_id = "candidate-sender-id";

    let mut steward_handle = setup_steward(
        conversation_id,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_id,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_bytes, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome(&welcome_bytes);

    // Add a third member proposal
    let joiner2 = setup_joiner(
        conversation_id,
        "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    );
    let result = process_inbound_compat(
        &mut steward_handle.group,
        Some(&mut steward_handle.mls),
        &joiner2.kp_packet.payload,
        WELCOME_SUBTOPIC,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => *gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id: ProposalId = 90;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner.group.insert_approved_proposal(proposal_id, gur);

    let packets = build_commit_candidate(
        &mut steward_handle.group,
        &mut steward_handle.mls,
        &steward_handle.steward_list,
        false,
        &steward_handle.member_id,
        b"test-app-id",
    )
    .unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Start freeze round before receiving candidate
    let epoch = joiner.mls.as_ref().unwrap().current_epoch().unwrap();
    joiner.group.start_freeze_round(epoch);

    // Joiner receives candidate — should buffer it
    let result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_mut(),
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::CommitCandidateReceived { .. }),
        "Expected CommitCandidateReceived, got {:?}",
        result
    );

    // Finalize: MLS commit staging authenticates the sender
    let joiner_id = joiner.self_member_id();
    let finalize = finalize_freeze_round(
        &mut joiner.group,
        joiner.mls.as_mut().unwrap(),
        &joiner.steward_list,
        false,
        false,
        &joiner_id,
    )
    .unwrap();
    let matched = matches!(
        &finalize.outcome,
        FreezeOutcome::Applied { result, .. } if matches!(*result, ProcessResult::ConversationUpdated)
    );
    assert!(
        matched,
        "Expected ConversationUpdated after finalize, got {finalize:?}"
    );
}

/// Test: when a backup steward commits in place of the live epoch steward_list,
/// finalize emits `CensorshipInactivity` against the absent steward.
///
/// Build a two-steward list over Alice + Bob. If hash rotation places the
/// OTHER steward at `epoch_steward(epoch)`, Bob's successful commit from
/// his own node pins the absent steward with `CensorshipInactivity`. If
/// rotation makes Bob himself the live epoch steward_list, the self-accusation
/// skip keeps the score-op list free of `CensorshipInactivity`.
#[test]
fn test_backup_commit_scores_absent_steward() {
    let conversation_id = "absent-steward";
    let alice_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let bob_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let charlie_hex = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";

    let mut alice_group = setup_steward(conversation_id, alice_hex);
    let mut bob = setup_joiner(conversation_id, bob_hex);
    let alice_id = alice_group.self_member_id().to_vec();
    let bob_id = bob.self_member_id();

    let (welcome_bytes, _) = steward_add_joiner(&mut alice_group, &bob.kp_packet);
    bob.accept_welcome(&welcome_bytes);

    // After the join, both are MLS members. Regenerate the steward list
    // over [Alice, Bob] so both are stewards at the current epoch.
    let epoch = alice_group.mls.current_epoch().unwrap();
    let members = vec![alice_id.clone(), bob_id.clone()];
    alice_group
        .steward_list
        .install_list(epoch, &members, 2, 0)
        .unwrap();
    bob.steward_list
        .install_list(epoch, &members, 2, 0)
        .unwrap();

    // Produce an approved proposal (invite Charlie) on both sides.
    let charlie = setup_joiner(conversation_id, charlie_hex);
    let gur = match process_inbound_compat(
        &mut alice_group.group,
        Some(&mut alice_group.mls),
        &charlie.kp_packet.payload,
        WELCOME_SUBTOPIC,
    )
    .unwrap()
    {
        ProcessResult::MembershipChangeReceived(g) => *g,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };
    let proposal_id: ProposalId = 90;
    alice_group.insert_approved_proposal(proposal_id, gur.clone());
    bob.group.insert_approved_proposal(proposal_id, gur);

    // Bob builds the commit (Alice never submits). His local candidate is
    // the only applicable entry when he finalises.
    let _ = build_commit_candidate(
        &mut bob.group,
        bob.mls.as_mut().unwrap(),
        &bob.steward_list,
        false,
        bob.member_id.member_id_bytes(),
        b"test-app-id",
    )
    .unwrap();
    let live_epoch_steward = {
        let eligible = bob.group.steward_eligibility(&members);
        bob.steward_list
            .epoch_steward(epoch, &eligible)
            .expect("both stewards are eligible")
            .to_vec()
    };

    let bob_id = bob.self_member_id();
    let result = finalize_freeze_round(
        &mut bob.group,
        bob.mls.as_mut().unwrap(),
        &bob.steward_list,
        false,
        false,
        &bob_id,
    )
    .unwrap();

    let matched = matches!(
        &result.outcome,
        FreezeOutcome::Applied { result: r, .. } if matches!(*r, ProcessResult::ConversationUpdated)
    );
    assert!(
        matched,
        "Expected ConversationUpdated, got {:?}",
        result.outcome
    );

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
            "No CensorshipInactivity expected when self is the live epoch steward_list, got {events:?}",
        );
    }
}

/// Test: a candidate whose wire `steward_member_id` doesn't match the
/// MLS-verified commit sender is dropped as `BrokenCommit`. Score is
/// attributed to the actual MLS sender, not the forged wire claim.
#[test]
fn test_forged_steward_member_id_scores_mls_sender() {
    let conversation_id = "forged-steward-id";

    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let third_hex = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";

    let mut steward_handle = setup_steward(conversation_id, steward_hex);
    let mut joiner = setup_joiner(conversation_id, joiner_hex);

    let (welcome_bytes, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome(&welcome_bytes);

    let third = setup_joiner(conversation_id, third_hex);
    let result = process_inbound_compat(
        &mut steward_handle.group,
        Some(&mut steward_handle.mls),
        &third.kp_packet.payload,
        WELCOME_SUBTOPIC,
    )
    .unwrap();
    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => *gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id: ProposalId = 91;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner.group.insert_approved_proposal(proposal_id, gur);

    let packets = build_commit_candidate(
        &mut steward_handle.group,
        &mut steward_handle.mls,
        &steward_handle.steward_list,
        false,
        &steward_handle.member_id,
        b"test-app-id",
    )
    .unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Forge `steward_member_id` to impersonate a different identity. The
    // MLS commit message is still signed by the real steward_list, so staging
    // succeeds and the cross-check fires.
    let mut app_msg = AppMessage::decode(batch_packet.payload.as_slice()).unwrap();
    let real_steward_id = steward_handle.self_member_id().to_vec();
    let forged_id = vec![0xCC; real_steward_id.len()];
    match &mut app_msg.payload {
        Some(app_message::Payload::CommitCandidate(c)) => {
            assert_eq!(
                c.steward_member_id, real_steward_id,
                "fixture: steward_member_id should start equal to MLS sender"
            );
            c.steward_member_id = forged_id.clone();
        }
        other => panic!("Expected CommitCandidate, got {:?}", other),
    }
    let mut forged_payload = Vec::with_capacity(app_msg.encoded_len());
    app_msg.encode(&mut forged_payload).unwrap();

    let result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_mut(),
        &forged_payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::CommitCandidateReceived { .. }),
        "Expected CommitCandidateReceived after wire forgery, got {:?}",
        result
    );

    let joiner_id = joiner.self_member_id();
    let finalize = finalize_freeze_round(
        &mut joiner.group,
        joiner.mls.as_mut().unwrap(),
        &joiner.steward_list,
        false,
        false,
        &joiner_id,
    )
    .unwrap();
    assert!(
        matches!(finalize.outcome, FreezeOutcome::NoCandidate),
        "Expected NoCandidate after dropping the forged candidate, got {:?}",
        finalize.outcome
    );

    let broken = finalize
        .score_ops
        .iter()
        .find(|op| matches!(op.event, ScoreEvent::BrokenCommit))
        .expect("Expected a BrokenCommit score op");
    assert_eq!(
        broken.member_id, real_steward_id,
        "score should target the MLS-verified commit sender, not the forged claim"
    );
    assert!(
        finalize
            .score_ops
            .iter()
            .all(|op| op.member_id != forged_id),
        "no score op should target the forged identity"
    );
}

/// Test: no valid candidate triggers NoCandidate result.
#[test]
fn test_no_valid_candidate_triggers_no_candidate() {
    let conversation_id = "no-candidate";

    let mut group = setup_steward(
        conversation_id,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );

    // Start freeze round with no candidates
    group.insert_approved_proposal(1, ConversationUpdateRequest { payload: None });
    let epoch = group.mls.current_epoch().unwrap();
    group.start_freeze_round(epoch);

    let group_id = group.self_member_id().to_vec();
    let finalize = finalize_freeze_round(
        &mut group.group,
        &mut group.mls,
        &group.steward_list,
        false,
        false,
        &group_id,
    )
    .unwrap();
    assert!(
        matches!(finalize.outcome, FreezeOutcome::NoCandidate),
        "Expected NoCandidate when no candidates buffered, got {:?}",
        finalize
    );
}
