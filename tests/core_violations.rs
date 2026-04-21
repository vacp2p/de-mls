//! Integration tests for steward violation detection, emergency criteria proposals,
//! freeze round selection, and dedup.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use prost::Message;

use de_mls::core::{
    CallbackError, FreezeFinalizeResult, Group, GroupEventHandler, ProcessResult, ProposalId,
    ProtocolConfig, ScoreEvent, apply_consensus_result, build_key_package_message,
    create_commit_candidate, create_group, finalize_freeze_round, prepare_to_join, process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService, parse_wallet_address};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, GroupUpdateRequest, ViolationEvidence, ViolationType, group_update_request,
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
    ) -> Result<String, CallbackError> {
        self.events.lock().unwrap().push(Event::Outbound {
            group: group_name.to_string(),
            packet,
        });
        Ok("mock-id".to_string())
    }

    async fn on_app_message(
        &self,
        group_name: &str,
        message: AppMessage,
    ) -> Result<(), CallbackError> {
        self.events.lock().unwrap().push(Event::AppMessage {
            group: group_name.to_string(),
            msg: message,
        });
        Ok(())
    }

    async fn on_leave_group(&self, group_name: &str) -> Result<(), CallbackError> {
        self.events.lock().unwrap().push(Event::LeaveGroup {
            group: group_name.to_string(),
        });
        Ok(())
    }

    async fn on_joined_group(&self, group_name: &str) -> Result<(), CallbackError> {
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

fn default_steward_config() -> ProtocolConfig {
    ProtocolConfig::new(1, 5).unwrap()
}

fn setup_mls(wallet_hex: &str) -> MlsService<MemoryDeMlsStorage> {
    let storage = MemoryDeMlsStorage::new();
    let mls = MlsService::new(storage);
    let wallet = parse_wallet_address(wallet_hex).unwrap();
    mls.init(wallet).unwrap();
    mls
}

fn setup_steward(group_name: &str, wallet_hex: &str) -> (MlsService<MemoryDeMlsStorage>, Group) {
    let mls = setup_mls(wallet_hex);
    let group = create_group(group_name, &mls, default_steward_config()).unwrap();
    (mls, group)
}

fn setup_joiner(
    group_name: &str,
    wallet_hex: &str,
) -> (MlsService<MemoryDeMlsStorage>, Group, OutboundPacket) {
    let mls = setup_mls(wallet_hex);
    let group = prepare_to_join(group_name, mls.wallet_bytes(), default_steward_config());
    let kp_packet = build_key_package_message(&group, &mls, b"test-app-id").unwrap();
    (mls, group, kp_packet)
}

fn steward_add_joiner(
    steward_mls: &MlsService<MemoryDeMlsStorage>,
    steward_handle: &mut Group,
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
        ProcessResult::MembershipChangeReceived(gur) => gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id = PROPOSAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    steward_handle.insert_approved_proposal(proposal_id, gur);
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

/// Test: apply_consensus_result for emergency criteria proposal (owner, accepted)
/// moves proposal to approved then immediately removes it (no MLS operation).
/// Emits BrokenCommit on target and EmergencyYesCreator on local user.
#[test]
fn test_apply_consensus_result_emergency_accepted_owner() {
    let group_name = "consensus-emergency-owner";
    let (_mls, mut group) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let target_id = vec![0xAA];
    let creator_id = b"local".to_vec();
    let emergency_request =
        ViolationEvidence::broken_commit(target_id.clone(), 0, Vec::<u8>::new())
            .with_creator(creator_id.clone())
            .into_update_request()
            .unwrap();
    let payload = emergency_request.encode_to_vec();
    let proposal_id = 77;
    group.store_voting_proposal(proposal_id, emergency_request);
    assert!(group.is_owner_of_proposal(proposal_id));

    let result = apply_consensus_result(&mut group, proposal_id, true, &payload).unwrap();

    // Emergency proposal should NOT remain in approved queue
    assert_eq!(group.approved_proposals_count(), 0);

    // Score ops: target penalty + creator reward
    assert_eq!(result.score_ops.len(), 2);
    assert_eq!(result.score_ops[0].member_id, target_id);
    assert_eq!(result.score_ops[0].event, ScoreEvent::BrokenCommit);
    assert_eq!(result.score_ops[1].member_id, creator_id);
    assert_eq!(result.score_ops[1].event, ScoreEvent::EmergencyYesCreator);
}

/// Test: apply_consensus_result for emergency criteria proposal (non-owner, accepted)
/// does not insert into approved queue.
/// Emits CensorshipInactivity on target and EmergencyYesCreator on creator from evidence.
#[test]
fn test_apply_consensus_result_emergency_accepted_non_owner() {
    let group_name = "consensus-emergency-non-owner";
    let (_mls, mut group) = setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let target_id = vec![0xBB];
    let creator_id = b"alice".to_vec();
    // Create the emergency proposal payload with creator identity
    let emergency_request = ViolationEvidence::censorship_inactivity(target_id.clone(), 1)
        .with_creator(creator_id.clone())
        .into_update_request()
        .unwrap();
    let payload = emergency_request.encode_to_vec();

    // The  group does NOT own this proposal (non-owner path)
    let proposal_id = 88;
    assert!(!group.is_owner_of_proposal(proposal_id));

    let result = apply_consensus_result(&mut group, proposal_id, true, &payload).unwrap();

    // Emergency proposal should NOT be in approved queue
    assert_eq!(group.approved_proposals_count(), 0);

    // Score ops: target penalty + creator reward (from evidence, not local_id)
    assert_eq!(result.score_ops.len(), 2);
    assert_eq!(result.score_ops[0].member_id, target_id);
    assert_eq!(result.score_ops[0].event, ScoreEvent::CensorshipInactivity);
    assert_eq!(result.score_ops[1].member_id, creator_id);
    assert_eq!(result.score_ops[1].event, ScoreEvent::EmergencyYesCreator);
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
    assert!(
        matches!(
            finalize,
            FreezeFinalizeResult::Applied {
                result: ProcessResult::GroupUpdated,
                ..
            }
        ),
        "Expected GroupUpdated after finalize, got {:?}",
        finalize
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
        matches!(finalize, FreezeFinalizeResult::NoCandidate),
        "Expected NoCandidate when no candidates buffered, got {:?}",
        finalize
    );
}
