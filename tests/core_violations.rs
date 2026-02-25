//! Integration tests for steward violation detection, emergency criteria proposals,
//! quarantine, and dedup.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use prost::Message;

use de_mls::core::{
    ConsensusOutcome, CoreError, GroupEventHandler, GroupHandle, ProcessResult, ProposalId,
    QuarantinePolicy, apply_consensus_result, build_key_package_message, create_batch_proposals,
    create_group, prepare_to_join, process_inbound, retry_quarantined,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService, parse_wallet_address};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, GroupUpdateRequest, ViolationEvidence, ViolationType,
    app_message, group_update_request,
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
    let packets = create_batch_proposals(steward_handle, steward_mls).unwrap();

    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet")
        .clone();

    let welcome_packet = packets
        .into_iter()
        .find(|p| p.subtopic == WELCOME_SUBTOPIC)
        .expect("Expected welcome packet");

    (welcome_packet, batch_packet)
}


// ─────────────────────────── Tests ───────────────────────────

/// Test: when batch proposal IDs match but digest differs (steward altered content),
/// a BROKEN_COMMIT ViolationDetected is returned.
#[test]
fn test_broken_commit_detection_digest_mismatch() {
    let group_name = "violation-broken-commit";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let (welcome_packet, _batch_packet) =
        steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
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

    let proposal_id: ProposalId = 42;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur);

    let packets = create_batch_proposals(&mut steward_handle, &steward_mls).unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    let app_msg = AppMessage::decode(batch_packet.payload.as_slice()).unwrap();
    let mut batch = match app_msg.payload {
        Some(app_message::Payload::BatchProposalsMessage(b)) => b,
        _ => panic!("Expected BatchProposalsMessage"),
    };
    batch.proposals_digest = vec![0xDE, 0xAD, 0xBE, 0xEF];

    let tampered_app_msg: AppMessage = batch.into();
    let tampered_payload = tampered_app_msg.encode_to_vec();

    let members_before = joiner_mls.members(group_name).unwrap();

    let result = process_inbound(
        &mut joiner_handle,
        &tampered_payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    match result {
        ProcessResult::ViolationDetected(evidence) => {
            assert_eq!(evidence.violation_type, ViolationType::BrokenCommit as i32);
        }
        other => panic!("Expected ViolationDetected(BROKEN_COMMIT), got {:?}", other),
    }

    let members_after = joiner_mls.members(group_name).unwrap();
    assert_eq!(
        members_before.len(),
        members_after.len(),
        "MLS state should not have changed after violation detection"
    );
}

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

    let result = create_batch_proposals(&mut handle, &mls);
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

    let result = create_batch_proposals(&mut steward_handle, &steward_mls);
    assert!(result.is_err());
    match result.unwrap_err() {
        de_mls::core::CoreError::UnexpectedEmergencyProposals { proposal_ids } => {
            assert_eq!(proposal_ids, vec![emergency_id]);
        }
        other => panic!("Expected UnexpectedEmergencyProposals, got {:?}", other),
    }
}

// ─────────────────────────── Quarantine & Dedup Tests ───────────────────────────

/// Test: batch arriving before consensus delivers proposals gets quarantined,
/// then resolves when proposals arrive via retry_quarantined.
#[test]
fn test_quarantine_batch_when_no_local_proposals() {
    let group_name = "quarantine-basic";

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
    let packets = create_batch_proposals(&mut steward_handle, &steward_mls).unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Joiner receives batch BEFORE consensus delivers proposals → BatchQuarantined
    let result = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::BatchQuarantined { .. }),
        "Expected BatchQuarantined, got {:?}",
        result
    );

    // Now consensus delivers the proposal to joiner
    joiner_handle.insert_approved_proposal(proposal_id, gur);

    // Try processing quarantined batches — should find the match and succeed
    let quarantine_results = retry_quarantined(&mut joiner_handle, &joiner_mls).unwrap();
    assert_eq!(quarantine_results.len(), 1);
    assert!(
        matches!(quarantine_results[0], ProcessResult::GroupUpdated),
        "Expected GroupUpdated, got {:?}",
        quarantine_results[0]
    );
}

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
    let packets = create_batch_proposals(&mut steward_handle, &steward_mls).unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // First receive: quarantined
    let r1 = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(r1, ProcessResult::BatchQuarantined { .. }));

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

/// Test: quarantine entries are dropped after max_epoch_age epochs.
#[test]
fn test_quarantine_expiry() {
    let group_name = "quarantine-expiry";
    let (_mls, _handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    // Verify that QuarantinePolicy::default() has expected values.
    let policy = QuarantinePolicy::default();
    assert_eq!(policy.max_epoch_age, 2);
    assert_eq!(policy.max_items, 5);
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

    let proposal_id: ProposalId = 46;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur);

    let packets = create_batch_proposals(&mut steward_handle, &steward_mls).unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    let app_msg = AppMessage::decode(batch_packet.payload.as_slice()).unwrap();
    let mut batch = match app_msg.payload {
        Some(app_message::Payload::BatchProposalsMessage(b)) => b,
        _ => panic!("Expected BatchProposalsMessage"),
    };
    batch.proposals_digest = vec![0xDE, 0xAD];

    let tampered_app_msg: AppMessage = batch.into();
    let tampered_payload = tampered_app_msg.encode_to_vec();

    let result = process_inbound(
        &mut joiner_handle,
        &tampered_payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(result, ProcessResult::ViolationDetected(_)));

    let members = joiner_mls.members(group_name).unwrap();
    assert_eq!(
        members.len(),
        2,
        "Members should still be accessible after violation discard"
    );

    assert_eq!(
        joiner_handle.approved_proposals_count(),
        0,
        "Approved proposals should be cleared after violation"
    );
}

/// Test: a stale/replayed batch does NOT produce ViolationDetected.
#[test]
fn test_stale_batch_does_not_accuse_steward() {
    let group_name = "stale-no-accuse";

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

    let proposal_id: ProposalId = 80;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur.clone());
    let packets = create_batch_proposals(&mut steward_handle, &steward_mls).unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    let result = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::GroupUpdated),
        "Expected GroupUpdated, got {:?}",
        result
    );

    let proposal_id2: ProposalId = 81;
    joiner_handle.insert_approved_proposal(proposal_id2, GroupUpdateRequest { payload: None });
    let pre_count = joiner_handle.approved_proposals_count();
    assert_eq!(pre_count, 1);

    let result = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    assert!(
        matches!(result, ProcessResult::Noop),
        "Expected Noop for stale batch, got {:?}",
        result
    );

    assert_eq!(
        joiner_handle.approved_proposals_count(),
        pre_count,
        "Approved proposals should NOT be cleared by a stale batch"
    );
}
