use super::*;

use crate::core::{build_key_package_message, create_group, prepare_to_join, process_inbound};
use crate::ds::WELCOME_SUBTOPIC;
use crate::mls_crypto::{MemoryDeMlsStorage, MlsService, parse_wallet_address};

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
    let kp_packet = build_key_package_message(&handle, &mls, b"test-app-id").unwrap();
    (mls, handle, kp_packet)
}

fn steward_add_joiner(
    steward_mls: &MlsService<MemoryDeMlsStorage>,
    steward_handle: &mut GroupHandle,
    joiner_kp_packet: &OutboundPacket,
) -> (OutboundPacket, OutboundPacket) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static PROPOSAL_COUNTER: AtomicU32 = AtomicU32::new(200);

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
    let app_id = b"test-app-id";
    let packets = create_commit_candidate(steward_handle, steward_mls, app_id).unwrap();

    let finalize = finalize_freeze_round(steward_handle, steward_mls, false, app_id).unwrap();
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

#[test]
fn test_validate_batch_proposals_action_mismatch() {
    let group_name = "validate-action-mismatch";
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

    let proposal_id: ProposalId = 43;
    joiner_handle.insert_approved_proposal(proposal_id, gur);

    let steward_id = steward_handle.steward_identity().unwrap().to_vec();
    let local_proposals = joiner_handle.approved_proposals();

    // Pass wrong MLS actions (empty) while local has an add proposal
    let result = validate_commit_candidate(
        &joiner_handle,
        &local_proposals,
        &steward_id,
        &[], // empty actions, should mismatch
    )
    .unwrap();

    match result {
        Some(ProcessResult::ViolationDetected(evidence)) => {
            use crate::protos::de_mls::messages::v1::ViolationType;
            assert_eq!(
                evidence.violation_type,
                ViolationType::BrokenMlsProposal as i32
            );
        }
        other => panic!("Expected Some(ViolationDetected), got {:?}", other),
    }
}
