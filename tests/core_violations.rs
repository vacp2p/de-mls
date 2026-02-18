//! Integration tests for steward violation detection and emergency criteria proposals.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

use de_mls::core::{
    CoreError, DefaultProvider, DispatchAction, GroupEventHandler, GroupHandle, ProcessResult,
    ProposalId, build_key_package_message, create_batch_proposals, create_group, dispatch_result,
    prepare_to_join, process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService, parse_wallet_address};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, BatchProposalsMessage, GroupUpdateRequest, ViolationEvidence, ViolationType,
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

    // Shortcut: insert directly as approved, bypassing consensus voting.
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

/// Compute the same digest as process_batch_proposals uses internally.
fn compute_proposals_digest(proposals: &HashMap<ProposalId, GroupUpdateRequest>) -> Vec<u8> {
    let sorted: BTreeMap<_, _> = proposals.iter().collect();
    let mut hasher = Sha256::new();
    for (&id, req) in &sorted {
        hasher.update(id.to_le_bytes());
        hasher.update(req.encode_to_vec());
    }
    hasher.finalize().to_vec()
}

// ─────────────────────────── Mock consensus ───────────────────────────

use hashgraph_like_consensus::{
    events::BroadcastEventBus, service::ConsensusService, storage::InMemoryConsensusStorage,
};

type TestConsensus =
    ConsensusService<String, InMemoryConsensusStorage<String>, BroadcastEventBus<String>>;

fn make_consensus() -> TestConsensus {
    let storage = InMemoryConsensusStorage::new();
    let event_bus = BroadcastEventBus::default();
    TestConsensus::new_with_components(storage, event_bus, 10)
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

    // Join the group
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

    // Set up a second joiner to create a real proposal
    let (_joiner2_mls, _joiner2_handle, kp2_packet) =
        setup_joiner(group_name, "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");

    // Steward processes key package
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

    // Shortcut: insert directly into both approved queues.
    // In production, the owner calls store_voting_proposal() → consensus votes →
    // handle_consensus_event() moves it to approved (owner path: mark_proposal_as_approved,
    // non-owner path: get_proposal_payload + insert_approved_proposal).
    // We skip the consensus round-trip here to isolate batch validation logic.
    let proposal_id: ProposalId = 42;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur);

    // Steward creates batch proposals
    let packets = create_batch_proposals(&mut steward_handle, &steward_mls).unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Decode the batch, tamper with the digest to simulate broken commit
    let app_msg = AppMessage::decode(batch_packet.payload.as_slice()).unwrap();
    let mut batch = match app_msg.payload {
        Some(app_message::Payload::BatchProposalsMessage(b)) => b,
        _ => panic!("Expected BatchProposalsMessage"),
    };
    batch.proposals_digest = vec![0xDE, 0xAD, 0xBE, 0xEF]; // Tampered digest

    let tampered_app_msg: AppMessage = batch.into();
    let tampered_payload = tampered_app_msg.encode_to_vec();

    // Joiner processes the tampered batch → should detect BROKEN_COMMIT
    let result = process_inbound(
        &mut joiner_handle,
        &tampered_payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    match result {
        ProcessResult::ViolationDetected(evidence) => {
            assert_eq!(evidence.violation_type, ViolationType::BrokenCommit as i32); // BROKEN_COMMIT
        }
        other => panic!("Expected ViolationDetected(BROKEN_COMMIT), got {:?}", other),
    }
}

/// Test: when MLS proposal count doesn't match expected count (IDs and digest match),
/// a BROKEN_MLS_PROPOSAL ViolationDetected is returned.
#[test]
fn test_broken_mls_proposal_count_mismatch() {
    let group_name = "violation-mls-count";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    // Join the group
    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    let join_result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    // Set up a second joiner
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

    // Shortcut: bypass consensus flow, insert directly into both approved queues.
    // See test_broken_commit_detection_digest_mismatch for explanation.
    let proposal_id: ProposalId = 43;
    steward_handle.insert_approved_proposal(proposal_id, gur.clone());
    joiner_handle.insert_approved_proposal(proposal_id, gur.clone());

    // Compute the correct digest from the joiner's perspective
    let mut proposals_map = HashMap::new();
    proposals_map.insert(proposal_id, gur);
    let correct_digest = compute_proposals_digest(&proposals_map);

    // Build a batch message with correct IDs and digest but wrong MLS proposal count
    let batch_msg = BatchProposalsMessage {
        group_name: group_name.as_bytes().to_vec(),
        mls_proposals: vec![], // Empty! Should have 1 proposal
        commit_message: vec![],
        proposal_ids: vec![proposal_id],
        proposals_digest: correct_digest,
    };
    let app_msg: AppMessage = batch_msg.into();
    let payload = app_msg.encode_to_vec();

    let result =
        process_inbound(&mut joiner_handle, &payload, APP_MSG_SUBTOPIC, &joiner_mls).unwrap();

    match result {
        ProcessResult::ViolationDetected(evidence) => {
            assert_eq!(
                evidence.violation_type,
                ViolationType::BrokenMlsProposal as i32
            ); // BROKEN_MLS_PROPOSAL
            let msg = String::from_utf8(evidence.evidence_payload).unwrap();
            assert!(msg.contains("expected 1 MLS proposals, got 0"));
        }
        other => panic!(
            "Expected ViolationDetected(BROKEN_MLS_PROPOSAL), got {:?}",
            other
        ),
    }
}

/// Test: ViolationDetected dispatches to StartVoting with EmergencyCriteriaProposal.
#[tokio::test]
async fn test_dispatch_violation_detected_starts_voting() {
    let group_name = "dispatch-violation";
    let (mls, handle) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let handler = MockHandler::new();
    let consensus = make_consensus();

    let evidence = ViolationEvidence::broken_commit(vec![0xAA, 0xBB], 5, vec![0xDE, 0xAD]);

    let action = dispatch_result::<DefaultProvider, _>(
        &handle,
        group_name,
        ProcessResult::ViolationDetected(evidence),
        &consensus,
        &handler,
        &mls,
    )
    .await
    .unwrap();

    match action {
        DispatchAction::StartVoting(request) => {
            // Verify it's an EmergencyCriteriaProposal
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
        other => panic!("Expected StartVoting, got {:?}", other),
    }
}

/// Test: proposal ID mismatch is detected as a BROKEN_COMMIT violation.
#[test]
fn test_proposal_id_mismatch_returns_violation() {
    let group_name = "violation-id-mismatch";

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

    // Send a batch with IDs that don't match joiner's local proposals
    let batch_msg = BatchProposalsMessage {
        group_name: group_name.as_bytes().to_vec(),
        mls_proposals: vec![],
        commit_message: vec![],
        proposal_ids: vec![999], // Joiner has no proposals
        proposals_digest: vec![],
    };
    let app_msg: AppMessage = batch_msg.into();
    let payload = app_msg.encode_to_vec();

    let result =
        process_inbound(&mut joiner_handle, &payload, APP_MSG_SUBTOPIC, &joiner_mls).unwrap();

    match result {
        ProcessResult::ViolationDetected(evidence) => {
            assert_eq!(evidence.violation_type, ViolationType::BrokenCommit as i32);
            let msg = String::from_utf8(evidence.evidence_payload).unwrap();
            assert!(msg.contains("proposal ID mismatch"));
        }
        other => panic!(
            "Expected ViolationDetected(BROKEN_COMMIT) for ID mismatch, got {:?}",
            other
        ),
    }
}

/// Test: create_batch_proposals rejects emergency criteria proposals in the approved queue.
/// Emergency proposals should be removed by handle_consensus_event before batch creation.
/// Their presence indicates a bug in the upstream flow.
#[test]
fn test_emergency_in_approved_queue_returns_error() {
    let group_name = "emergency-only-batch";
    let (mls, mut handle) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let emergency_request =
        ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new()).into_update_request();
    handle.insert_approved_proposal(50, emergency_request);
    assert_eq!(handle.approved_proposals_count(), 1);

    // Emergency proposals in the approved queue are an invariant violation.
    let result = create_batch_proposals(&mut handle, &mls);
    assert!(result.is_err());
    match result.unwrap_err() {
        de_mls::core::CoreError::UnexpectedEmergencyProposals { proposal_ids } => {
            assert_eq!(proposal_ids, vec![50]);
        }
        other => panic!(
            "Expected UnexpectedEmergencyProposals, got {:?}",
            other
        ),
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

/// Test: ViolationEvidence carries correct target_member_id (steward identity)
/// and epoch from GroupHandle when processing a tampered batch.
#[test]
fn test_violation_evidence_carries_steward_id_and_epoch() {
    let group_name = "violation-evidence-fields";
    let steward_wallet = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    let (steward_mls, mut steward_handle) = setup_steward(group_name, steward_wallet);
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    // Join the group
    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    // Set the steward identity on joiner's handle (normally set during protocol flow)
    let steward_wallet_addr = parse_wallet_address(steward_wallet).unwrap();
    joiner_handle.set_steward_identity(steward_wallet_addr.as_slice().to_vec());

    // Advance epoch a few times to verify epoch tracking
    joiner_handle.advance_epoch(); // epoch 1
    joiner_handle.advance_epoch(); // epoch 2
    assert_eq!(joiner_handle.current_epoch(), 2);

    // Send a batch with mismatched IDs to trigger a violation
    let batch_msg = BatchProposalsMessage {
        group_name: group_name.as_bytes().to_vec(),
        mls_proposals: vec![],
        commit_message: vec![],
        proposal_ids: vec![999],
        proposals_digest: vec![],
    };
    let app_msg: AppMessage = batch_msg.into();
    let payload = app_msg.encode_to_vec();

    let result =
        process_inbound(&mut joiner_handle, &payload, APP_MSG_SUBTOPIC, &joiner_mls).unwrap();

    match result {
        ProcessResult::ViolationDetected(evidence) => {
            assert_eq!(evidence.violation_type, ViolationType::BrokenCommit as i32);
            // target_member_id should be the steward's wallet bytes
            assert_eq!(
                evidence.target_member_id,
                steward_wallet_addr.as_slice().to_vec()
            );
            // epoch should reflect current epoch on the handle
            assert_eq!(evidence.epoch, 2);
        }
        other => panic!("Expected ViolationDetected, got {:?}", other),
    }
}

/// Test: epoch counter increments when approved proposals are cleared (commit processed).
#[test]
fn test_epoch_advances_on_clear_approved_proposals() {
    let group_name = "epoch-advance";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    assert_eq!(handle.current_epoch(), 0);

    // Insert and clear to simulate a commit
    handle.insert_approved_proposal(1, GroupUpdateRequest { payload: None });
    handle.clear_approved_proposals();
    assert_eq!(handle.current_epoch(), 1);

    handle.insert_approved_proposal(2, GroupUpdateRequest { payload: None });
    handle.clear_approved_proposals();
    assert_eq!(handle.current_epoch(), 2);

    // advance_epoch also works independently
    handle.advance_epoch();
    assert_eq!(handle.current_epoch(), 3);
}

/// Test: handle_consensus_event for emergency criteria proposal (owner, accepted)
/// moves proposal to approved then immediately removes it (no MLS operation).
#[tokio::test]
async fn test_consensus_event_emergency_accepted_owner() {
    let group_name = "consensus-emergency-owner";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let consensus = make_consensus();

    // Create an emergency criteria proposal in voting queue (simulates owner creating it)
    let emergency_request =
        ViolationEvidence::broken_commit(vec![0xAA], 0, Vec::<u8>::new()).into_update_request();
    let proposal_id = 77;
    handle.store_voting_proposal(proposal_id, emergency_request);
    assert!(handle.is_owner_of_proposal(proposal_id));

    // Simulate consensus YES
    let event = hashgraph_like_consensus::types::ConsensusEvent::ConsensusReached {
        proposal_id,
        result: true,
        timestamp: 12345,
    };

    de_mls::core::handle_consensus_event::<DefaultProvider>(
        &mut handle,
        group_name,
        event,
        &consensus,
    )
    .await
    .unwrap();

    // Emergency proposal should NOT remain in approved queue
    assert_eq!(handle.approved_proposals_count(), 0);
}

/// Test: handle_consensus_event for emergency criteria proposal (non-owner, accepted)
/// does not insert into approved queue.
#[tokio::test]
async fn test_consensus_event_emergency_accepted_non_owner() {
    let group_name = "consensus-emergency-non-owner";
    let (_mls, mut handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    // Use a real consensus service that can store the proposal payload
    let consensus = make_consensus();

    // Create the emergency proposal payload
    let emergency_request =
        ViolationEvidence::censorship_inactivity(vec![0xBB], 1).into_update_request();
    let payload = emergency_request.encode_to_vec();
    // Store the proposal in the consensus service so get_proposal_payload works
    use hashgraph_like_consensus::api::ConsensusServiceAPI;
    use hashgraph_like_consensus::types::CreateProposalRequest;
    let scope = group_name.to_string();
    let create_req =
        CreateProposalRequest::new("test".to_string(), payload, "voter1".into(), 1, 3600, true)
            .unwrap();
    let created = consensus.create_proposal(&scope, create_req).await.unwrap();

    // The handle does NOT own this proposal (non-owner path)
    assert!(!handle.is_owner_of_proposal(created.proposal_id));

    // Simulate consensus YES
    let event = hashgraph_like_consensus::types::ConsensusEvent::ConsensusReached {
        proposal_id: created.proposal_id,
        result: true,
        timestamp: 12345,
    };

    de_mls::core::handle_consensus_event::<DefaultProvider>(
        &mut handle,
        group_name,
        event,
        &consensus,
    )
    .await
    .unwrap();

    // Emergency proposal should NOT be in approved queue
    assert_eq!(handle.approved_proposals_count(), 0);
}

/// Test: emergency proposals mixed with regular proposals in approved queue
/// causes an error — they should have been removed by handle_consensus_event.
#[test]
fn test_emergency_mixed_with_regular_returns_error() {
    let group_name = "emergency-digest-filter";
    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    // Join the group
    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    // Create a real add-member proposal
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

    // Add both a regular proposal and an emergency proposal to steward.
    // This simulates a bug where handle_consensus_event didn't clean up
    // the emergency proposal from the approved queue.
    let regular_id: ProposalId = 60;
    let emergency_id: ProposalId = 61;
    steward_handle.insert_approved_proposal(regular_id, gur.clone());
    steward_handle.insert_approved_proposal(
        emergency_id,
        ViolationEvidence::broken_commit(vec![], 0, Vec::<u8>::new()).into_update_request(),
    );

    // Batch creation should fail because emergency proposals should not be
    // in the approved queue — handle_consensus_event should have removed them.
    let result = create_batch_proposals(&mut steward_handle, &steward_mls);
    assert!(result.is_err());
    match result.unwrap_err() {
        de_mls::core::CoreError::UnexpectedEmergencyProposals { proposal_ids } => {
            assert_eq!(proposal_ids, vec![emergency_id]);
        }
        other => panic!(
            "Expected UnexpectedEmergencyProposals, got {:?}",
            other
        ),
    }
}
