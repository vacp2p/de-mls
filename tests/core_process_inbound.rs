//! Integration tests for `process_inbound`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use prost::Message;

use de_mls::core::{
    CallbackError, CoreError, FreezeFinalizeResult, Group, GroupEventHandler, ProcessResult,
    ProtocolConfig, StewardList, build_key_package_message, build_message, create_commit_candidate,
    create_group, finalize_freeze_round, group_members, prepare_to_join, process_inbound,
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

fn setup_mls(wallet_hex: &str) -> MlsService<MemoryDeMlsStorage> {
    let storage = MemoryDeMlsStorage::new();
    let mls = MlsService::new(storage);
    let wallet = parse_wallet_address(wallet_hex).unwrap();
    mls.init(wallet).unwrap();
    mls
}

fn default_steward_config() -> ProtocolConfig {
    ProtocolConfig::new(1, 5).unwrap()
}

/// Create group as steward, return (mls, group).
fn setup_steward(group_name: &str, wallet_hex: &str) -> (MlsService<MemoryDeMlsStorage>, Group) {
    let mls = setup_mls(wallet_hex);
    let group = create_group(group_name, &mls, default_steward_config()).unwrap();
    (mls, group)
}

/// Create group as steward with custom config, return (mls, group).
fn setup_steward_with_config(
    group_name: &str,
    wallet_hex: &str,
    config: ProtocolConfig,
) -> (MlsService<MemoryDeMlsStorage>, Group) {
    let mls = setup_mls(wallet_hex);
    let group = create_group(group_name, &mls, config).unwrap();
    (mls, group)
}

/// Prepare a joiner: create MlsService, prepare group, build key-package packet.
fn setup_joiner(
    group_name: &str,
    wallet_hex: &str,
) -> (MlsService<MemoryDeMlsStorage>, Group, OutboundPacket) {
    let mls = setup_mls(wallet_hex);
    let group = prepare_to_join(group_name, mls.wallet_bytes(), default_steward_config());
    let kp_packet = build_key_package_message(&group, &mls, b"test-app-id").unwrap();
    (mls, group, kp_packet)
}

/// Prepare a joiner with custom config: create MlsService, prepare group, build key-package packet.
fn setup_joiner_with_config(
    group_name: &str,
    wallet_hex: &str,
    config: ProtocolConfig,
) -> (MlsService<MemoryDeMlsStorage>, Group, OutboundPacket) {
    let mls = setup_mls(wallet_hex);
    let group = prepare_to_join(group_name, mls.wallet_bytes(), config);
    let kp_packet = build_key_package_message(&group, &mls, b"test-app-id").unwrap();
    (mls, group, kp_packet)
}

// Full join flow: steward adds joiner, returns welcome packet for joiner.
fn steward_add_joiner(
    steward_mls: &MlsService<MemoryDeMlsStorage>,
    steward_handle: &mut Group,
    joiner_kp_packet: &OutboundPacket,
) -> OutboundPacket {
    use std::sync::atomic::{AtomicU32, Ordering};
    static PROPOSAL_COUNTER: AtomicU32 = AtomicU32::new(1);

    // 1. Steward processes key package → MembershipChangeReceived
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

    // 2. Insert as approved (skip voting in tests) and create batch
    let proposal_id = PROPOSAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    steward_handle.insert_approved_proposal(proposal_id, gur);
    let _packets = create_commit_candidate(steward_handle, steward_mls, b"test-app-id").unwrap();

    let finalize =
        finalize_freeze_round(steward_handle, steward_mls, false, b"test-app-id").unwrap();
    match finalize {
        FreezeFinalizeResult::Applied { result, outbound } => {
            assert!(
                matches!(result, ProcessResult::GroupUpdated),
                "Expected GroupUpdated, got {:?}",
                result
            );
            // Welcome is now deferred to finalize_freeze_round
            outbound
                .into_iter()
                .find(|p| p.subtopic == WELCOME_SUBTOPIC)
                .expect("Expected a deferred welcome packet from finalize_freeze_round")
        }
        other => panic!("Expected Applied, got {:?}", other),
    }
}

// ─────────────────────────── process_inbound tests ───────────────────────────

#[test]
fn test_process_inbound_invalid_subtopic() {
    let (mls, mut group) =
        setup_steward("test-group", "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let result = process_inbound(&mut group, b"some payload", "invalid", &mls);
    assert!(result.is_err());
    match result.unwrap_err() {
        CoreError::InvalidSubtopic(s) => assert_eq!(s, "invalid"),
        e => panic!("Expected InvalidSubtopic, got {:?}", e),
    }
}

#[test]
fn test_process_inbound_app_msg_before_mls_init() {
    let mls = setup_mls("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let mut group = prepare_to_join("test-group", mls.wallet_bytes(), default_steward_config());

    let result = process_inbound(&mut group, b"some payload", APP_MSG_SUBTOPIC, &mls).unwrap();
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
    let outbound = build_message(&steward_handle, &steward_mls, &app_msg, b"test-app-id").unwrap();

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
        ProcessResult::MembershipChangeReceived(gur) => {
            assert!(gur.payload.is_some(), "Expected InviteMember payload");
        }
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    }
}

#[test]
fn test_process_inbound_welcome_non_steward_ignores_key_package() {
    let group_name = "non-steward-kp";

    let mls = setup_mls("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let mut group = prepare_to_join(group_name, mls.wallet_bytes(), default_steward_config());

    let (_joiner_mls, _joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let result = process_inbound(&mut group, &kp_packet.payload, WELCOME_SUBTOPIC, &mls).unwrap();

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
            assert!(joiner_mls.has_group(joiner_handle.group_name()));
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
    steward_handle.insert_approved_proposal(2, remove_req.clone());
    joiner_handle.insert_approved_proposal(2, remove_req);
    let packets =
        create_commit_candidate(&mut steward_handle, &steward_mls, b"test-app-id").unwrap();

    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    // Start freeze round before receiving candidate
    let epoch = joiner_mls.current_epoch(group_name).unwrap();
    joiner_handle.start_freeze_round(epoch);

    let remove_result = process_inbound(
        &mut joiner_handle,
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    assert!(
        matches!(remove_result, ProcessResult::CommitCandidateReceived),
        "Expected CommitCandidateReceived, got {:?}",
        remove_result
    );

    let finalize =
        finalize_freeze_round(&mut joiner_handle, &joiner_mls, false, b"test-app-id").unwrap();
    assert!(
        matches!(
            finalize,
            FreezeFinalizeResult::Applied {
                result: ProcessResult::LeaveGroup,
                ..
            }
        ),
        "Expected LeaveGroup after finalize, got {:?}",
        finalize
    );
}

#[test]
fn test_process_inbound_raw_commit_payload_is_ignored() {
    let group_name = "raw-commit-ignored";

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
    steward_handle.insert_approved_proposal(7, remove_req);
    let packets =
        create_commit_candidate(&mut steward_handle, &steward_mls, b"test-app-id").unwrap();

    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    let app_msg = AppMessage::decode(batch_packet.payload.as_slice()).unwrap();
    let raw_commit = match app_msg.payload {
        Some(app_message::Payload::CommitCandidate(c)) => c.commit_message,
        _ => panic!("Expected CommitCandidate payload"),
    };

    let result = process_inbound(
        &mut joiner_handle,
        &raw_commit,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(result, ProcessResult::Noop));
}

// ─────────────────────────── Auto-fill steward list tests ───────────────────────────

/// After a commit that adds a member, if member_count < sn_min the steward list
/// should be auto-filled with all members. With sn_min=3, a 2-member group triggers
/// auto-fill; once a third member joins, auto-fill no longer triggers.
#[test]
fn test_auto_fill_steward_list_triggers_below_sn_min() {
    let group = "auto-fill-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner1_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let joiner2_hex = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";

    // sn_min=3: auto-fill when member_count < 3
    let config = ProtocolConfig::new(3, 5).unwrap();

    let (steward_mls, mut steward_handle) =
        setup_steward_with_config(group, steward_hex, config.clone());

    // Bootstrap: 1 member < sn_min=3 → auto-fill should trigger
    let members_before = group_members(&steward_handle, &steward_mls).unwrap();
    assert_eq!(members_before.len(), 1);
    assert!(members_before.len() < config.sn_min);

    // Add first joiner
    let (_joiner1_mls, _joiner1_handle, joiner1_kp) =
        setup_joiner_with_config(group, joiner1_hex, config.clone());
    steward_add_joiner(&steward_mls, &mut steward_handle, &joiner1_kp);

    // 2 members: still below sn_min=3 → auto-fill should trigger
    let members_2 = group_members(&steward_handle, &steward_mls).unwrap();
    assert_eq!(members_2.len(), 2);
    assert!(members_2.len() < config.sn_min);
    let epoch = steward_mls.current_epoch(group).unwrap();
    let sn = members_2.len().min(config.sn_max);
    assert!(
        steward_handle
            .generate_and_set_steward_list(epoch, &members_2, sn)
            .is_ok()
    );

    // Verify the steward list was regenerated with all 2 members
    let list = steward_handle
        .steward_list()
        .expect("steward list should exist after auto-fill");
    assert_eq!(list.len(), 2);
    for m in &members_2 {
        assert!(
            list.contains(m),
            "auto-filled list should contain all members"
        );
    }

    // Add second joiner
    let (_joiner2_mls, _joiner2_handle, joiner2_kp) =
        setup_joiner_with_config(group, joiner2_hex, config.clone());
    steward_add_joiner(&steward_mls, &mut steward_handle, &joiner2_kp);

    // 3 members: at sn_min=3 → auto-fill should NOT trigger
    let members_3 = group_members(&steward_handle, &steward_mls).unwrap();
    assert_eq!(members_3.len(), 3);
    assert!(
        members_3.len() >= config.sn_min,
        "auto-fill should not trigger when member_count >= sn_min"
    );
}

/// With default config (sn_min=1), auto-fill never triggers because
/// member_count is always >= 1.
#[test]
fn test_auto_fill_never_triggers_with_default_config() {
    let group = "no-auto-fill-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    let (steward_mls, steward_handle) = setup_steward(group, steward_hex);

    let members = group_members(&steward_handle, &steward_mls).unwrap();
    assert_eq!(members.len(), 1);
    assert!(
        members.len() >= steward_handle.protocol_config().sn_min,
        "default config (sn_min=1) should never trigger auto-fill"
    );
}

// ─────────────────────────── Steward list sync tests ───────────────────────────

/// Steward sends a StewardListSync app message after adding a joiner.
/// The joiner receives it, validates it, and applies the steward list.
#[test]
fn test_steward_list_sync_roundtrip() {
    use de_mls::protos::de_mls::messages::v1::StewardListSync;

    let group_name = "sync-list-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let (steward_mls, mut steward_handle) = setup_steward(group_name, steward_hex);
    let (joiner_mls, mut joiner_handle, kp_packet) = setup_joiner(group_name, joiner_hex);

    // Steward adds joiner
    let welcome_packet = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);

    // Joiner processes welcome
    let join_result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    // Joiner has no steward list yet
    assert!(
        joiner_handle.steward_list().is_none(),
        "Joiner should not have a steward list before sync"
    );

    // Steward builds the sync message from its list
    let steward_list = steward_handle
        .steward_list()
        .expect("steward should have a list");
    let sync = StewardListSync {
        steward_members: steward_list.members().to_vec(),
        start_epoch: steward_list.start_epoch(),
        sn_min: steward_list.config().sn_min as u32,
        sn_max: steward_list.config().sn_max as u32,
    };
    let app_msg: AppMessage = sync.clone().into();
    let sync_packet =
        build_message(&steward_handle, &steward_mls, &app_msg, b"test-app-id").unwrap();

    // Joiner processes the encrypted sync message
    let result = process_inbound(
        &mut joiner_handle,
        &sync_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();

    // Should get StewardListSyncReceived
    match &result {
        ProcessResult::StewardListSyncReceived(received_sync) => {
            assert_eq!(received_sync.steward_members, sync.steward_members);
            assert_eq!(received_sync.start_epoch, sync.start_epoch);
            assert_eq!(received_sync.sn_min, sync.sn_min);
            assert_eq!(received_sync.sn_max, sync.sn_max);
        }
        other => panic!("Expected StewardListSyncReceived, got {:?}", other),
    }

    // Simulate app-layer handling: validate and apply
    if let ProcessResult::StewardListSyncReceived(sync) = result {
        let config = ProtocolConfig::new(sync.sn_min as usize, sync.sn_max as usize).unwrap();
        let members = group_members(&joiner_handle, &joiner_mls).unwrap();

        // Validate: all steward members are current group members
        let all_present = sync
            .steward_members
            .iter()
            .all(|sm| members.iter().any(|m| m == sm));
        assert!(
            all_present,
            "All steward members should be current group members"
        );

        // Validate: ordering is correct (self-consistent hash sort)
        assert!(
            StewardList::validate(
                &sync.steward_members,
                sync.start_epoch,
                group_name.as_bytes(),
                &sync.steward_members,
                &config,
            )
            .is_ok(),
            "Received steward list ordering should be valid"
        );

        let sn = sync.steward_members.len();
        assert!(
            joiner_handle
                .generate_and_set_steward_list(sync.start_epoch, &sync.steward_members, sn,)
                .is_ok()
        );
    }

    // Joiner now has the steward list
    let joiner_list = joiner_handle
        .steward_list()
        .expect("Joiner should have a steward list after sync");
    let steward_list = steward_handle
        .steward_list()
        .expect("steward should have a list");
    assert_eq!(joiner_list.members(), steward_list.members());
    assert_eq!(joiner_list.start_epoch(), steward_list.start_epoch());
}

/// If a member already has a steward list, receiving a sync message is a no-op.
#[test]
fn test_steward_list_sync_idempotent_for_existing_members() {
    use de_mls::protos::de_mls::messages::v1::StewardListSync;

    let group_name = "sync-idempotent-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let (steward_mls, mut steward_handle) = setup_steward(group_name, steward_hex);
    let (joiner_mls, mut joiner_handle, kp_packet) = setup_joiner(group_name, joiner_hex);

    // Steward adds joiner
    let welcome_packet = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);

    // Joiner processes welcome
    let join_result = process_inbound(
        &mut joiner_handle,
        &welcome_packet.payload,
        WELCOME_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(matches!(join_result, ProcessResult::JoinedGroup(_)));

    // Manually give the joiner a steward list (simulating a previous sync)
    let members = group_members(&joiner_handle, &joiner_mls).unwrap();
    assert!(
        joiner_handle
            .generate_and_set_steward_list(0, &members, 1)
            .is_ok()
    );
    assert!(joiner_handle.steward_list().is_some());

    // Now steward sends a sync message
    let steward_list = steward_handle.steward_list().unwrap();
    let sync = StewardListSync {
        steward_members: steward_list.members().to_vec(),
        start_epoch: steward_list.start_epoch(),
        sn_min: steward_list.config().sn_min as u32,
        sn_max: steward_list.config().sn_max as u32,
    };
    let app_msg: AppMessage = sync.into();
    let sync_packet =
        build_message(&steward_handle, &steward_mls, &app_msg, b"test-app-id").unwrap();

    // Joiner processes sync — should get StewardListSyncReceived at the core level
    let result = process_inbound(
        &mut joiner_handle,
        &sync_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::StewardListSyncReceived(_)),
        "Core should still return StewardListSyncReceived"
    );

    // But the app layer would skip it because the list is already set.
    // We verify the existing list wasn't overwritten (still 1 member, not steward's list).
    assert_eq!(
        joiner_handle.steward_list().unwrap().len(),
        1,
        "Existing list should be preserved (1 member), not overwritten by sync"
    );
}
