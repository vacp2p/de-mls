//! Integration tests for `process_inbound`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use prost::Message;

use de_mls::core::{
    CoreError, GroupEventHandler, GroupHandle, ProcessResult,
    build_key_package_message, build_message, create_batch_proposals, create_group,
    prepare_to_join, process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService, parse_wallet_address};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, ConversationMessage, GroupUpdateRequest, app_message,
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

/// Create group as steward, return (mls, handle).
fn setup_steward(
    group_name: &str,
    wallet_hex: &str,
) -> (MlsService<MemoryDeMlsStorage>, GroupHandle) {
    let mls = setup_mls(wallet_hex);
    let handle = create_group(group_name, &mls).unwrap();
    (mls, handle)
}

/// Prepare a joiner: create MlsService, prepare handle, build key-package packet.
fn setup_joiner(
    group_name: &str,
    wallet_hex: &str,
) -> (MlsService<MemoryDeMlsStorage>, GroupHandle, OutboundPacket) {
    let mls = setup_mls(wallet_hex);
    let handle = prepare_to_join(group_name);
    let kp_packet = build_key_package_message(&handle, &mls).unwrap();
    (mls, handle, kp_packet)
}

// Full join flow: steward adds joiner, returns welcome packet for joiner.
fn steward_add_joiner(
    steward_mls: &MlsService<MemoryDeMlsStorage>,
    steward_handle: &mut GroupHandle,
    joiner_kp_packet: &OutboundPacket,
) -> OutboundPacket {
    use std::sync::atomic::{AtomicU32, Ordering};
    static PROPOSAL_COUNTER: AtomicU32 = AtomicU32::new(1);

    // 1. Steward processes key package → GetUpdateRequest
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

    // 2. Insert as approved (skip voting in tests) and create batch
    let proposal_id = PROPOSAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    steward_handle.insert_approved_proposal(proposal_id, gur);
    let packets = create_batch_proposals(steward_handle, steward_mls).unwrap();

    // Find the welcome packet
    packets
        .into_iter()
        .find(|p| p.subtopic == WELCOME_SUBTOPIC)
        .expect("Expected a welcome packet from create_batch_proposals")
}

// ─────────────────────────── process_inbound tests ───────────────────────────

#[test]
fn test_process_inbound_invalid_subtopic() {
    let (mls, mut handle) =
        setup_steward("test-group", "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let result = process_inbound(&mut handle, b"some payload", "invalid", &mls);
    assert!(result.is_err());
    match result.unwrap_err() {
        CoreError::InvalidSubtopic(s) => assert_eq!(s, "invalid"),
        e => panic!("Expected InvalidSubtopic, got {:?}", e),
    }
}

#[test]
fn test_process_inbound_app_msg_before_mls_init() {
    let mls = setup_mls("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let mut handle = prepare_to_join("test-group");

    let result = process_inbound(&mut handle, b"some payload", APP_MSG_SUBTOPIC, &mls).unwrap();
    assert!(matches!(result, ProcessResult::Noop));
}

#[test]
fn test_process_inbound_conversation_message_roundtrip() {
    let group_name = "roundtrip-group";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let welcome_packet = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);

    let join_result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    let conv = ConversationMessage {
        message: b"Hello from steward!".to_vec(),
        sender: "steward".to_string(),
        group_name: group_name.to_string(),
    };
    let app_msg: AppMessage = conv.into();
    let outbound = build_message(&steward_handle, &steward_mls, &app_msg).unwrap();

    let result = process_inbound(
        &mut joiner_handle,
        &outbound.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    match result {
        ProcessResult::AppMessage(msg) => {
            let payload = msg.payload.expect("Expected payload");
            match payload {
                app_message::Payload::ConversationMessage(cm) => {
                    assert_eq!(cm.message, b"Hello from steward!");
                    assert_eq!(cm.sender, "steward");
                }
                _ => panic!("Expected ConversationMessage payload"),
            }
        }
        other => panic!("Expected AppMessage, got {:?}", other),
    }
}

#[test]
fn test_process_inbound_welcome_steward_receives_key_package() {
    let group_name = "steward-kp-group";

    let (_steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (_joiner_mls, _joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let result = process_inbound(
        &mut steward_handle,
        &kp_packet.payload,
        WELCOME_SUBTOPIC,
        &_steward_mls,
    )
    .unwrap();

    match result {
        ProcessResult::GetUpdateRequest(gur) => {
            assert!(gur.payload.is_some(), "Expected InviteMember payload");
        }
        other => panic!("Expected GetUpdateRequest, got {:?}", other),
    }
}

#[test]
fn test_process_inbound_welcome_non_steward_ignores_key_package() {
    let group_name = "non-steward-kp";

    let mls = setup_mls("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let mut handle = prepare_to_join(group_name);

    let (_joiner_mls, _joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let result = process_inbound(&mut handle, &kp_packet.payload, WELCOME_SUBTOPIC, &mls).unwrap();

    assert!(
        matches!(result, ProcessResult::Noop),
        "Expected Noop, got {:?}",
        result
    );
}

#[test]
fn test_process_inbound_welcome_invitation_joins_group() {
    let group_name = "join-group";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let welcome_packet = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);

    let result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    match result {
        ProcessResult::JoinedGroup(name) => {
            assert_eq!(name, group_name);
            assert!(joiner_handle.is_mls_initialized());
        }
        other => panic!("Expected JoinedGroup, got {:?}", other),
    }
}

#[test]
fn test_process_inbound_welcome_already_joined_ignores() {
    let group_name = "already-joined";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let welcome_packet = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    let result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(result, ProcessResult::JoinedGroup(_)));

    let (_joiner2_mls, _joiner2_handle, kp2_packet) =
        setup_joiner(group_name, "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
    let welcome_packet2 = steward_add_joiner(&steward_mls, &mut steward_handle, &kp2_packet);

    let result2 = process_inbound(
        &mut joiner_handle,
        &welcome_packet2.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(result2, ProcessResult::Noop),
        "Expected Noop for already joined, got {:?}",
        result2
    );
}

#[test]
fn test_process_inbound_leave_group() {
    let group_name = "leave-group";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let welcome_packet = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
    let result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(result, ProcessResult::JoinedGroup(_)));

    let joiner_wallet = parse_wallet_address("0x70997970C51812dc3A010C7d01b50e0d17dc79C8").unwrap();
    let remove_req = GroupUpdateRequest {
        payload: Some(
            de_mls::protos::de_mls::messages::v1::group_update_request::Payload::RemoveMember(
                de_mls::protos::de_mls::messages::v1::RemoveMember {
                    identity: joiner_wallet.as_slice().to_vec(),
                },
            ),
        ),
    };
    steward_handle.insert_approved_proposal(2, remove_req);
    let packets = create_batch_proposals(&mut steward_handle, &steward_mls).unwrap();

    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    let app_msg = AppMessage::decode(batch_packet.payload.as_slice()).unwrap();
    let batch = match app_msg.payload {
        Some(app_message::Payload::BatchProposalsMessage(b)) => b,
        _ => panic!("Expected BatchProposalsMessage"),
    };

    for proposal_bytes in &batch.mls_proposals {
        let _r = joiner_mls.decrypt(group_name, proposal_bytes).unwrap();
    }

    let remove_result = process_inbound(
        &mut joiner_handle,
        &batch.commit_message,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    assert!(
        matches!(remove_result, ProcessResult::LeaveGroup),
        "Expected LeaveGroup, got {:?}",
        remove_result
    );
}

#[test]
fn test_process_inbound_batch_quarantined_when_no_local_proposals() {
    let group_name = "batch-quarantine";

    let (steward_mls, mut steward_handle) =
        setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let (joiner_mls, mut joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let welcome_packet = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
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

    let proposal_id = 44u32;
    steward_handle.insert_approved_proposal(proposal_id, gur);
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
    assert!(de_mls::core::has_quarantined(&joiner_handle));
}
