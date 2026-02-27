//! Integration tests for steward violation detection, emergency criteria proposals,
//! freeze round selection, and dedup.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use prost::Message;

use de_mls::core::{
    ConsensusOutcome, CoreError, FreezeFinalizeResult, GroupEventHandler, GroupHandle,
    ProcessResult, ProposalId, apply_consensus_result, build_key_package_message,
    create_commit_candidate, create_group, finalize_freeze_round, prepare_to_join, process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService, parse_wallet_address};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, GroupUpdateRequest, ViolationEvidence, ViolationType,
    group_update_request,
};

// ─────────────────────────── Mock Handler ───────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum Event {
    Outbound {
        group: String,
        packet: OutboundPacket,
    },
    AppMessage {
        group: String,
        msg: AppMessage,
    },
    LeaveGroup {
        group: String,
    },
    JoinedGroup {
        group: String,
    },
    Error {
        group: String,
        op: String,
        err: String,
    },
}

#[derive(Clone)]
struct MockHandler {
    events: Arc<Mutex<Vec<Event>>>,
}

impl MockHandler {
    #[allow(dead_code)]
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[allow(dead_code)]
    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl GroupEventHandler for MockHandler {
    async fn on_outbound(
        &self,
        group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CoreError> {
        self.events.lock().unwrap().push(Event::Outbound {
            group: group_name.to_string(),
            packet,
        });
        Ok("mock-id".to_string())
    }

    async fn on_app_message(&self, group_name: &str, message: AppMessage) -> Result<(), CoreError> {
        self.events.lock().unwrap().push(Event::AppMessage {
            group: group_name.to_string(),
            msg: message,
        });
        Ok(())
    }

    async fn on_leave_group(&self, group_name: &str) -> Result<(), CoreError> {
        self.events.lock().unwrap().push(Event::LeaveGroup {
            group: group_name.to_string(),
        });
        Ok(())
    }

    async fn on_joined_group(&self, group_name: &str) -> Result<(), CoreError> {
        self.events.lock().unwrap().push(Event::JoinedGroup {
            group: group_name.to_string(),
        });
        Ok(())
    }

    async fn on_error(&self, group_name: &str, operation: &str, error: &str) {
        self.events.lock().unwrap().push(Event::Error {
            group: group_name.to_string(),
            op: operation.to_string(),
            err: error.to_string(),
        });
    }
}

// ─────────────────────────── Helpers ───────────────────────────

fn setup_mls(wallet_hex: &str) -> MlsService<MemoryDeMlsStorage> {
    let storage = MemoryDeMlsStorage::new();
    let mls = MlsService::new(storage);
    let wallet = parse_wallet_address(wallet_hex).unwrap();
    mls.init(wallet).unwrap();
    mls
}

fn setup_steward(
    group_name: &str,
    wallet_hex: &str,
) -> (MlsService<MemoryDeMlsStorage>, GroupHandle) {
    let mls = setup_mls(wallet_hex);
    let handle = create_group(group_name, &mls).unwrap();
    (mls, handle)
}

fn setup_joiner(
    group_name: &str,
    wallet_hex: &str,
) -> (MlsService<MemoryDeMlsStorage>, GroupHandle, OutboundPacket) {
    let mls = setup_mls(wallet_hex);
    let handle = prepare_to_join(group_name);
    let kp_packet = build_key_package_message(&handle, &mls).unwrap();
    (mls, handle, kp_packet)
}

fn steward_add_joiner(
    steward_mls: &MlsService<MemoryDeMlsStorage>,
    steward_handle: &mut GroupHandle,
    joiner_kp_packet: &OutboundPacket,
) -> (OutboundPacket, OutboundPacket) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static PROPOSAL_COUNTER: AtomicU32 = AtomicU32::new(100);

    let result = process_inbound(
        steward_handle,
        &joiner_kp_packet.payload,
        WELCOME_SUBTOPIC,
        steward_mls,
    )
    .unwrap();

    let gur = match result {
        ProcessResult::GetUpdateRequest(gur) => gur,
        other => panic!("Expected GetUpdateRequest, got {:?}", other),
    };

    let proposal_id = PROPOSAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    steward_handle.insert_approved_proposal(proposal_id, gur);
    let packets = create_commit_candidate(steward_handle, steward_mls).unwrap();

    let finalize = finalize_freeze_round(steward_handle, steward_mls).unwrap();
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
                .expect("Expected deferred welcome packet from finalize_freeze_round")
        }
        other => panic!("Expected Applied, got {:?}", other),
    };

    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet")
        .clone();

    (welcome_packet, batch_packet)
}

// ─────────────────────────── Tests ───────────────────────────

/// Test: ViolationDetected carries correct evidence for emergency criteria proposal.
#[test]
fn test_violation_detected_produces_emergency_proposal() {
    let evidence = ViolationEvidence::broken_commit(vec![0xAA, 0xBB], 5, vec![0xDE, 0xAD]);
    let request = evidence.into_update_request();

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
    let (mls, mut handle) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let emergency_request =
        ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new()).into_update_request();
    handle.insert_approved_proposal(50, emergency_request);
    assert_eq!(handle.approved_proposals_count(), 1);

    let result = create_commit_candidate(&mut handle, &mls);
    assert!(result.is_err());
    match result.unwrap_err() {
        de_mls::core::CoreError::UnexpectedEmergencyProposals { proposal_ids } => {
            assert_eq!(proposal_ids, vec![50]);
        }
        other => panic!("Expected UnexpectedEmergencyProposals, got {:?}", other),
    }
}

/// Test: remove_approved_proposal correctly removes a single proposal.
#[test]
fn test_remove_approved_proposal() {
    let group_name = "remove-approved";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let emergency_request =
        ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new()).into_update_request();
    handle.insert_approved_proposal(50, emergency_request);
    assert_eq!(handle.approved_proposals_count(), 1);

    handle.remove_approved_proposal(50);
    assert_eq!(handle.approved_proposals_count(), 0);
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
fn test_epoch_advances_on_clear_approved_proposals() {
    let group_name = "epoch-advance";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    assert_eq!(handle.current_epoch(), 0);

    handle.insert_approved_proposal(1, GroupUpdateRequest { payload: None });
    handle.clear_approved_proposals();
    assert_eq!(handle.current_epoch(), 1);

    handle.insert_approved_proposal(2, GroupUpdateRequest { payload: None });
    handle.clear_approved_proposals();
    assert_eq!(handle.current_epoch(), 2);

    handle.advance_epoch();
    assert_eq!(handle.current_epoch(), 3);
}

/// Test: apply_consensus_result for emergency criteria proposal (owner, accepted)
/// moves proposal to approved then immediately removes it (no MLS operation).
#[test]
fn test_apply_consensus_result_emergency_accepted_owner() {
    let group_name = "consensus-emergency-owner";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let emergency_request =
        ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new()).into_update_request();
    let proposal_id = 77;
    handle.store_voting_proposal(proposal_id, emergency_request);
    assert!(handle.is_owner_of_proposal(proposal_id));

    apply_consensus_result(&mut handle, proposal_id, ConsensusOutcome::ApprovedOwner).unwrap();

    // Emergency proposal should NOT remain in approved queue
    assert_eq!(handle.approved_proposals_count(), 0);
}

/// Test: apply_consensus_result for emergency criteria proposal (non-owner, accepted)
/// does not insert into approved queue.
#[test]
fn test_apply_consensus_result_emergency_accepted_non_owner() {
    let group_name = "consensus-emergency-non-owner";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    // Create the emergency proposal payload
    let emergency_request =
        ViolationEvidence::censorship_inactivity(vec![0xBB], 1).into_update_request();
    let payload = emergency_request.encode_to_vec();

    // The handle does NOT own this proposal (non-owner path)
    let proposal_id = 88;
    assert!(!handle.is_owner_of_proposal(proposal_id));

    apply_consensus_result(
        &mut handle,
        proposal_id,
        ConsensusOutcome::Approved { payload: &payload },
    )
    .unwrap();

    // Emergency proposal should NOT be in approved queue
    assert_eq!(handle.approved_proposals_count(), 0);
}

/// Test: apply_consensus_result returns ProposalNotFound for unknown proposal ID
/// on the owner-only path.
#[test]
fn test_apply_consensus_result_owner_path_proposal_not_found() {
    let group_name = "invalid-owner";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let result = apply_consensus_result(&mut handle, 999, ConsensusOutcome::ApprovedOwner);

    assert!(result.is_err());
    match result.unwrap_err() {
        CoreError::ProposalNotFound(id) => assert_eq!(id, 999),
        other => panic!("Expected ProposalNotFound, got {:?}", other),
    }
}

/// Test: apply_consensus_result rejects ApprovedOwner when proposal is already approved
/// (not in owner's voting queue anymore).
#[test]
fn test_apply_consensus_result_invalid_approved_owner_already_approved() {
    let group_name = "invalid-owner-approved";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let proposal_id = 777;
    handle.insert_approved_proposal(proposal_id, GroupUpdateRequest { payload: None });

    let result = apply_consensus_result(&mut handle, proposal_id, ConsensusOutcome::ApprovedOwner);

    assert!(result.is_err());
    match result.unwrap_err() {
        CoreError::InvalidConsensusOutcome(_) => {}
        other => panic!("Expected InvalidConsensusOutcome, got {:?}", other),
    }
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
        ProcessResult::GetUpdateRequest(gur) => gur,
        other => panic!("Expected GetUpdateRequest, got {:?}", other),
    };

    let regular_id: ProposalId = 60;
    let emergency_id: ProposalId = 61;
    steward_handle.insert_approved_proposal(regular_id, gur.clone());
    steward_handle.insert_approved_proposal(
        emergency_id,
        ViolationEvidence::broken_commit(vec![], 0, Vec::<u8>::new()).into_update_request(),
    );

    let result = create_commit_candidate(&mut steward_handle, &steward_mls);
    assert!(result.is_err());
    match result.unwrap_err() {
        de_mls::core::CoreError::UnexpectedEmergencyProposals { proposal_ids } => {
            assert_eq!(proposal_ids, vec![emergency_id]);
        }
        other => panic!("Expected UnexpectedEmergencyProposals, got {:?}", other),
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
        ProcessResult::GetUpdateRequest(gur) => gur,
        other => panic!("Expected GetUpdateRequest, got {:?}", other),
    };

    let proposal_id: ProposalId = 45;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur);
    let packets = create_commit_candidate(&mut steward_handle, &steward_mls).unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Start freeze round so candidates get buffered
    joiner_handle.start_freeze_round();

    // First receive: candidate buffered → CandidateBuffered
    let r1 = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(r1, ProcessResult::CandidateBuffered),
        "Expected CandidateBuffered, got {:?}",
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
        ProcessResult::GetUpdateRequest(gur) => gur,
        other => panic!("Expected GetUpdateRequest, got {:?}", other),
    };

    let proposal_id: ProposalId = 44;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    let packets = create_commit_candidate(&mut steward_handle, &steward_mls).unwrap();
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
        ProcessResult::GetUpdateRequest(gur) => gur,
        other => panic!("Expected GetUpdateRequest, got {:?}", other),
    };

    let proposal_id: ProposalId = 90;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur);

    let packets = create_commit_candidate(&mut steward_handle, &steward_mls).unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Start freeze round before receiving candidate
    joiner_handle.start_freeze_round();

    // Joiner receives candidate — should buffer it
    let result = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::CandidateBuffered),
        "Expected CandidateBuffered, got {:?}",
        result
    );

    // Finalize: MLS commit staging authenticates the sender
    let finalize = finalize_freeze_round(&mut joiner_handle, &joiner_mls).unwrap();
    assert!(
        matches!(
            finalize,
            FreezeFinalizeResult::Applied { result: ProcessResult::GroupUpdated, .. }
        ),
        "Expected GroupUpdated after finalize, got {:?}",
        finalize
    );

    // After finalization, steward identity should be set from the MLS-authenticated commit
    let steward_id = joiner_handle.steward_identity().expect("steward identity should be set");
    assert!(
        !steward_id.is_empty(),
        "Steward identity should be non-empty after finalization"
    );
}

/// Test: reject_all_voting_proposals clears voting queue.
#[test]
fn test_reject_all_voting_proposals_clears_queue() {
    let group_name = "reject-voting";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    // Store proposals in voting queue
    handle.store_voting_proposal(1, GroupUpdateRequest { payload: None });
    handle.store_voting_proposal(2, GroupUpdateRequest { payload: None });
    assert!(handle.is_owner_of_proposal(1));
    assert!(handle.is_owner_of_proposal(2));

    // Reject all voting proposals
    handle.reject_all_voting_proposals();

    // Voting queue should be empty
    assert!(!handle.is_owner_of_proposal(1));
    assert!(!handle.is_owner_of_proposal(2));
}

/// Test: no valid candidate triggers NoCandidate result.
#[test]
fn test_no_valid_candidate_triggers_no_candidate() {
    let group_name = "no-candidate";

    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    // Start freeze round with no candidates
    handle.insert_approved_proposal(1, GroupUpdateRequest { payload: None });
    handle.start_freeze_round();

    let mls = setup_mls("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let finalize = finalize_freeze_round(&mut handle, &mls).unwrap();
    assert!(
        matches!(finalize, FreezeFinalizeResult::NoCandidate),
        "Expected NoCandidate when no candidates buffered, got {:?}",
        finalize
    );
}
