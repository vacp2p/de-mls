//! Integration tests for the core inbound pipeline.
//!
//! Covers the app-message decrypt path, the steward-side key-package
//! intake (now surfaced via the test-only [`process_inbound_compat`]
//! shim), the joiner welcome-acceptance flow, and the ConversationSync handling
//! that flows through `decrypt_application_only`.

use std::sync::Arc;

use prost::Message;

use de_mls::core::{
    Conversation, CoreError, FreezeOutcome, ProcessResult, StewardList, StewardListConfig,
    StewardListPlugin, build_key_package_message, finalize_freeze_round,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use de_mls::identity::{Identity, parse_wallet_address};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService, OpenMlsService};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, ConversationMessage, ConversationUpdateRequest, app_message,
};

mod common;
use common::{
    build_commit_candidate, default_steward_config, process_inbound_compat, setup_identity_storage,
    setup_joiner, setup_joiner_with_config, setup_steward, setup_steward_with_config,
    steward_add_joiner,
};

// ─────────────────────────── process_inbound tests ───────────────────────────

#[test]
fn test_process_inbound_invalid_subtopic() {
    let mut group = setup_steward("test-group", "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let result = process_inbound_compat(&mut group, None, b"some payload", "invalid");
    assert!(result.is_err());
    match result.unwrap_err() {
        CoreError::InvalidSubtopic(s) => assert_eq!(s, "invalid"),
        e => panic!("Expected InvalidSubtopic, got {:?}", e),
    }
}

#[test]
fn test_process_inbound_app_msg_before_mls_init() {
    // Joiner-side handle with no MLS service attached yet.
    let (identity, _credentials, _storage) =
        setup_identity_storage("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let mut group: Conversation =
        Conversation::prepare_to_join("test-group", identity.identity_bytes().to_vec());

    let result =
        process_inbound_compat(&mut group, None, b"some payload", APP_MSG_SUBTOPIC).unwrap();
    assert!(matches!(result, ProcessResult::Noop));
}

#[test]
fn test_process_inbound_conversation_message_roundtrip() {
    let conversation_name = "roundtrip-group";

    let mut steward_handle = setup_steward(
        conversation_name,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_name,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    let conv = ConversationMessage {
        message: b"Hello from steward!".to_vec(),
        sender: "steward".to_string(),
        conversation_name: conversation_name.to_string(),
    };
    let app_msg: AppMessage = conv.into();
    let outbound = steward_handle
        .mls
        .build_message(&app_msg, b"test-app-id")
        .unwrap();

    let result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_ref(),
        &outbound.payload,
        APP_MSG_SUBTOPIC,
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
    let conversation_name = "steward-kp-group";

    let mut steward_handle = setup_steward(
        conversation_name,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let joiner = setup_joiner(
        conversation_name,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let result = process_inbound_compat(
        &mut steward_handle.group,
        Some(&steward_handle.mls),
        &joiner.kp_packet.payload,
        WELCOME_SUBTOPIC,
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
fn test_process_inbound_welcome_non_steward_buffers_key_package() {
    let conversation_name = "non-steward-kp";

    // Joiner-side handle with no MLS yet — the key-package surfaces all the
    // same; promotion to a voting proposal is the app's decision.
    let (identity, _credentials, _storage) =
        setup_identity_storage("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let mut group: Conversation =
        Conversation::prepare_to_join(conversation_name, identity.identity_bytes().to_vec());

    let other = setup_joiner(
        conversation_name,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let result =
        process_inbound_compat(&mut group, None, &other.kp_packet.payload, WELCOME_SUBTOPIC)
            .unwrap();

    assert!(
        matches!(result, ProcessResult::MembershipChangeReceived(_)),
        "Expected MembershipChangeReceived, got {:?}",
        result
    );
}

#[test]
fn test_process_inbound_welcome_invitation_joins_group() {
    let conversation_name = "join-group";

    let mut steward_handle = setup_steward(
        conversation_name,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_name,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    // Joiner now has a live MLS service for the right group.
    let mls = joiner.mls.as_ref().expect("welcome attaches MLS service");
    assert_eq!(mls.conversation_id(), conversation_name);
}

#[test]
fn test_process_inbound_welcome_already_joined_ignores() {
    let conversation_name = "already-joined";

    let mut steward_handle = setup_steward(
        conversation_name,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_name,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    // A second welcome (this one for joiner2's KP) doesn't address us, so
    // try_accept_welcome surfaces "not for us" rather than disturbing our
    // MLS state. The User-layer dispatch additionally guards on
    // `group.mls().is_some()` to skip wholesale; both safeguards land at
    // the same outcome.
    let mut joiner2 = setup_joiner(
        conversation_name,
        "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC",
    );
    let (welcome_packet2, _) = steward_add_joiner(&mut steward_handle, &joiner2.kp_packet);

    let invitation = match prost::Message::decode(welcome_packet2.payload.as_slice())
        .map(|w: de_mls::protos::de_mls::messages::v1::WelcomeMessage| w.payload)
        .unwrap()
    {
        Some(de_mls::protos::de_mls::messages::v1::welcome_message::Payload::InvitationToJoin(
            inv,
        )) => inv,
        other => panic!("Expected InvitationToJoin, got {:?}", other),
    };
    let outcome = joiner
        .try_accept_welcome(&invitation.mls_message_out_bytes)
        .expect("non-matching welcomes parse cleanly");
    assert!(
        outcome.is_none(),
        "second welcome targets joiner2's KP, must not produce a service for joiner1"
    );

    // Joiner2 actually accepts theirs as a sanity check.
    joiner2.accept_welcome_packet(&welcome_packet2);
    assert!(joiner2.mls.is_some());
}

#[test]
fn test_process_inbound_leave_group() {
    let conversation_name = "leave-group";

    let mut steward_handle = setup_steward(
        conversation_name,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_name,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    let joiner_wallet = parse_wallet_address("0x70997970C51812dc3A010C7d01b50e0d17dc79C8").unwrap();
    let remove_req = ConversationUpdateRequest {
        payload: Some(
            de_mls::protos::de_mls::messages::v1::conversation_update_request::Payload::RemoveMember(
                de_mls::protos::de_mls::messages::v1::RemoveMember {
                    identity: joiner_wallet.as_slice().to_vec(),
                },
            ),
        ),
    };
    steward_handle.insert_approved_proposal(2, remove_req.clone());
    joiner.group.insert_approved_proposal(2, remove_req);
    let packets = build_commit_candidate(
        &mut steward_handle.group,
        &steward_handle.mls,
        &steward_handle.steward,
        false,
        &steward_handle.identity,
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

    let remove_result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_ref(),
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();

    assert!(
        matches!(remove_result, ProcessResult::CommitCandidateReceived),
        "Expected CommitCandidateReceived, got {:?}",
        remove_result
    );

    let finalize = finalize_freeze_round(
        &mut joiner.group,
        joiner.mls.as_ref().unwrap(),
        &joiner.steward,
        false,
        false,
        b"test-app-id",
    )
    .unwrap();
    let matched = matches!(
        &finalize.outcome,
        FreezeOutcome::Applied { result, .. } if matches!(**result, ProcessResult::LeaveConversation)
    );
    assert!(
        matched,
        "Expected LeaveConversation after finalize, got {finalize:?}"
    );
}

/// Test: an evicted member can rejoin the same conversation_id in the same
/// session. The steward commits a removal, the joiner finalizes it as
/// `LeaveConversation`, then `Conversation::take_mls().delete()` clears storage so the
/// next welcome creates a fresh handle without colliding with the dead
/// post-eviction state.
#[test]
fn test_rejoin_after_eviction() {
    let conversation_name = "rejoin-after-eviction";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let joiner_id = parse_wallet_address(joiner_hex)
        .unwrap()
        .as_slice()
        .to_vec();

    // Phase 1: joiner joins.
    let mut steward_handle = setup_steward(conversation_name, steward_hex);
    let mut joiner = setup_joiner(conversation_name, joiner_hex);
    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);
    assert!(joiner.mls.as_ref().is_some());

    // Phase 2: steward commits a removal of the joiner. Both sides apply
    // the commit; the joiner's finalize emits `LeaveConversation`.
    let remove_req = ConversationUpdateRequest {
        payload: Some(
            de_mls::protos::de_mls::messages::v1::conversation_update_request::Payload::RemoveMember(
                de_mls::protos::de_mls::messages::v1::RemoveMember {
                    identity: joiner_id.clone(),
                },
            ),
        ),
    };
    steward_handle.insert_approved_proposal(2, remove_req.clone());
    joiner.group.insert_approved_proposal(2, remove_req);
    let packets = build_commit_candidate(
        &mut steward_handle.group,
        &steward_handle.mls,
        &steward_handle.steward,
        false,
        &steward_handle.identity,
        b"test-app-id",
    )
    .unwrap();
    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    let epoch_before_remove = joiner.mls.as_ref().unwrap().current_epoch().unwrap();
    joiner.group.start_freeze_round(epoch_before_remove);

    process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_ref(),
        &batch_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    let finalize_joiner = finalize_freeze_round(
        &mut joiner.group,
        joiner.mls.as_ref().unwrap(),
        &joiner.steward,
        false,
        false,
        b"test-app-id",
    )
    .unwrap();
    assert!(
        matches!(
            &finalize_joiner.outcome,
            FreezeOutcome::Applied { result, .. } if matches!(**result, ProcessResult::LeaveConversation)
        ),
        "Expected LeaveConversation on joiner finalize, got {finalize_joiner:?}"
    );
    let finalize_steward = finalize_freeze_round(
        &mut steward_handle.group,
        &steward_handle.mls,
        &steward_handle.steward,
        false,
        false,
        b"test-app-id",
    )
    .unwrap();
    assert!(matches!(
        &finalize_steward.outcome,
        FreezeOutcome::Applied { result, .. } if matches!(**result, ProcessResult::ConversationUpdated)
    ));
    assert!(!steward_handle.mls.is_member(&joiner_id),);

    // Phase 3: app-layer cleanup that follows `ProcessResult::LeaveConversation`.
    // Take the MLS service out of the joiner handle and tear down its
    // storage; the second take is a no-op (idempotent).
    let removed = joiner.mls.take().expect("mls present pre-leave");
    removed.delete().unwrap();
    drop(removed);
    assert!(joiner.mls.is_none());
    assert!(joiner.mls.take().is_none(), "second take is idempotent");

    // Phase 4: joiner generates a fresh KP (re-using the same identity +
    // storage); the steward re-adds them. With MLS now on the entry/handle
    // rather than `Conversation`, no separate joiner-side `Conversation` allocation is
    // needed for the rejoin — the test only needs the resulting MLS service.
    let _ = default_steward_config();
    let key_package = OpenMlsService::<Arc<MemoryDeMlsStorage>>::generate_key_package(
        &joiner.storage,
        &joiner.credentials,
    )
    .unwrap();
    let kp_packet = build_key_package_message(conversation_name, key_package, b"test-app-id");
    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &kp_packet);

    // Manually accept the welcome (we can't reuse JoinerHandle because we
    // built `joiner_group` directly; the helper takes the same path).
    let invitation = match prost::Message::decode(welcome_packet.payload.as_slice())
        .map(|w: de_mls::protos::de_mls::messages::v1::WelcomeMessage| w.payload)
        .unwrap()
    {
        Some(de_mls::protos::de_mls::messages::v1::welcome_message::Payload::InvitationToJoin(
            inv,
        )) => inv,
        other => panic!("Expected InvitationToJoin, got {:?}", other),
    };
    let joiner_mls = OpenMlsService::new_from_welcome(
        &invitation.mls_message_out_bytes,
        Arc::clone(&joiner.storage),
        Arc::clone(&joiner.credentials),
    )
    .unwrap()
    .expect("welcome should match this joiner's fresh KP");

    // Phase 5: both sides see the rejoined member at a strictly-later epoch.
    let epoch_after_rejoin = joiner_mls.current_epoch().unwrap();
    assert!(
        epoch_after_rejoin > epoch_before_remove,
        "Rejoin should land at a later epoch ({epoch_after_rejoin} > {epoch_before_remove})"
    );
    assert!(steward_handle.mls.is_member(&joiner_id));
    assert!(joiner_mls.is_member(&joiner_id));
    assert!(joiner_mls.is_member(steward_handle.self_identity()),);
}

#[test]
fn test_process_inbound_raw_commit_payload_is_ignored() {
    let conversation_name = "raw-commit-ignored";

    let mut steward_handle = setup_steward(
        conversation_name,
        "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
    );
    let mut joiner = setup_joiner(
        conversation_name,
        "0x70997970C51812dc3A010C7d01b50e0d17dc79C8",
    );

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    let joiner_wallet = parse_wallet_address("0x70997970C51812dc3A010C7d01b50e0d17dc79C8").unwrap();
    let remove_req = ConversationUpdateRequest {
        payload: Some(
            de_mls::protos::de_mls::messages::v1::conversation_update_request::Payload::RemoveMember(
                de_mls::protos::de_mls::messages::v1::RemoveMember {
                    identity: joiner_wallet.as_slice().to_vec(),
                },
            ),
        ),
    };
    steward_handle.insert_approved_proposal(7, remove_req);
    let packets = build_commit_candidate(
        &mut steward_handle.group,
        &steward_handle.mls,
        &steward_handle.steward,
        false,
        &steward_handle.identity,
        b"test-app-id",
    )
    .unwrap();

    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet");

    let app_msg = AppMessage::decode(batch_packet.payload.as_slice()).unwrap();
    let raw_commit = match app_msg.payload {
        Some(app_message::Payload::CommitCandidate(c)) => c.commit_message,
        _ => panic!("Expected CommitCandidate payload"),
    };

    let result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_ref(),
        &raw_commit,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    assert!(matches!(result, ProcessResult::Noop));
}

// ─────────────────────────── Auto-fill steward list tests ───────────────────────────

#[test]
fn test_auto_fill_steward_list_triggers_below_sn_min() {
    let group = "auto-fill-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner1_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let joiner2_hex = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";

    // sn_min=3: auto-fill when member_count < 3
    let config = StewardListConfig::new(3, 5).unwrap();

    let mut steward_handle = setup_steward_with_config(group, steward_hex, config.clone());

    // Bootstrap: 1 member < sn_min=3
    let members_before = steward_handle.mls.members().unwrap();
    assert_eq!(members_before.len(), 1);
    assert!(members_before.len() < config.sn_min);

    // Add first joiner
    let joiner1 = setup_joiner_with_config(group, joiner1_hex, config.clone());
    steward_add_joiner(&mut steward_handle, &joiner1.kp_packet);

    // 2 members: still below sn_min=3
    let members_2 = steward_handle.mls.members().unwrap();
    assert_eq!(members_2.len(), 2);
    assert!(members_2.len() < config.sn_min);
    let epoch = steward_handle.mls.current_epoch().unwrap();
    let sn = members_2.len().min(config.sn_max);
    assert!(
        steward_handle
            .steward
            .install_list(epoch, &members_2, sn, 0)
            .is_ok()
    );

    let list = steward_handle
        .steward
        .current_list()
        .expect("steward list should exist after auto-fill");
    assert_eq!(list.len(), 2);
    for m in &members_2 {
        assert!(
            list.contains(m),
            "auto-filled list should contain all members"
        );
    }

    // Add second joiner
    let joiner2 = setup_joiner_with_config(group, joiner2_hex, config.clone());
    steward_add_joiner(&mut steward_handle, &joiner2.kp_packet);

    // 3 members: at sn_min=3
    let members_3 = steward_handle.mls.members().unwrap();
    assert_eq!(members_3.len(), 3);
    assert!(
        members_3.len() >= config.sn_min,
        "auto-fill should not trigger when member_count >= sn_min"
    );
}

#[test]
fn test_auto_fill_never_triggers_with_default_config() {
    let group = "no-auto-fill-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

    let steward_handle = setup_steward(group, steward_hex);

    let members = steward_handle.mls.members().unwrap();
    assert_eq!(members.len(), 1);
    assert!(
        members.len() >= steward_handle.steward.config().sn_min,
        "default config (sn_min=1) should never trigger auto-fill"
    );
}

// ─────────────────────────── Conversation sync tests ───────────────────────────

#[test]
fn test_group_sync_roundtrip() {
    use de_mls::protos::de_mls::messages::v1::ConversationSync;

    let conversation_name = "sync-list-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let mut steward_handle = setup_steward(conversation_name, steward_hex);
    let mut joiner = setup_joiner(conversation_name, joiner_hex);

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    assert!(
        joiner.steward.current_list().is_none(),
        "Joiner should not have a steward list before sync"
    );

    let steward_list = steward_handle
        .steward
        .current_list()
        .expect("steward should have a list");
    let sync = ConversationSync {
        steward_members: steward_list.members().to_vec(),
        election_epoch: steward_list.election_epoch(),
        sn_min: steward_list.config().sn_min as u32,
        sn_max: steward_list.config().sn_max as u32,
        allow_subset_candidates: false,
        peer_scores: vec![],
        timing: None,
        retry_round: 0,
        max_reelection_attempts: 1,
        liveness_criteria_yes: true,
        threshold_peer_score: 0,
        pending_update_max_epochs: 3,
    };
    let app_msg: AppMessage = sync.clone().into();
    let sync_packet = steward_handle
        .mls
        .build_message(&app_msg, b"test-app-id")
        .unwrap();

    let result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_ref(),
        &sync_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();

    match &result {
        ProcessResult::ConversationSyncReceived(received_sync) => {
            assert_eq!(received_sync.steward_members, sync.steward_members);
            assert_eq!(received_sync.election_epoch, sync.election_epoch);
            assert_eq!(received_sync.sn_min, sync.sn_min);
            assert_eq!(received_sync.sn_max, sync.sn_max);
        }
        other => panic!("Expected GroupSyncReceived, got {:?}", other),
    }

    if let ProcessResult::ConversationSyncReceived(sync) = result {
        let config = StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize).unwrap();
        let members = joiner.mls.as_ref().unwrap().members().unwrap();

        let all_present = sync
            .steward_members
            .iter()
            .all(|sm| members.iter().any(|m| m == sm));
        assert!(
            all_present,
            "All steward members should be current group members"
        );

        assert!(
            StewardList::validate(
                &sync.steward_members,
                sync.election_epoch,
                conversation_name.as_bytes(),
                &sync.steward_members,
                &config,
                sync.retry_round,
            )
            .is_ok(),
            "Received steward list ordering should be valid"
        );

        let sn = sync.steward_members.len();
        assert!(
            joiner
                .steward
                .install_list(
                    sync.election_epoch,
                    &sync.steward_members,
                    sn,
                    sync.retry_round,
                )
                .is_ok()
        );
    }

    let joiner_list = joiner
        .steward
        .current_list()
        .expect("Joiner should have a steward list after sync");
    let steward_list = steward_handle
        .steward
        .current_list()
        .expect("steward should have a list");
    assert_eq!(joiner_list.members(), steward_list.members());
    assert_eq!(joiner_list.election_epoch(), steward_list.election_epoch());
}

#[test]
fn test_group_sync_propagates_divergent_per_group_config() {
    use de_mls::app::InMemoryPeerScoreStorage;
    use de_mls::core::{PeerScoringPlugin, PeerScoringService, ScoreSnapshot};
    use de_mls::core::{ScoreEvent, ScoreOp, ScoringConfig};
    use de_mls::protos::de_mls::messages::v1::{ConversationSync, PeerScore};

    const STEWARD_THRESHOLD: i64 = -50;
    const STEWARD_LIVENESS_YES: bool = false;
    const STEWARD_PENDING_MAX_EPOCHS: u32 = 11;
    const STEWARD_SN_MIN: usize = 2;
    const STEWARD_SN_MAX: usize = 8;

    let conversation_name = "sync-divergent-config";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let steward_protocol = StewardListConfig::new(STEWARD_SN_MIN, STEWARD_SN_MAX).unwrap();
    let mut steward_handle =
        setup_steward_with_config(conversation_name, steward_hex, steward_protocol);
    let mut joiner =
        setup_joiner_with_config(conversation_name, joiner_hex, default_steward_config());

    steward_handle.liveness_criteria_yes = STEWARD_LIVENESS_YES;
    steward_handle.pending_update_max_epochs = STEWARD_PENDING_MAX_EPOCHS;

    assert_ne!(joiner.liveness_criteria_yes, STEWARD_LIVENESS_YES);
    assert_ne!(joiner.pending_update_max_epochs, STEWARD_PENDING_MAX_EPOCHS);
    assert_ne!(joiner.steward.config().sn_min, STEWARD_SN_MIN);
    assert_ne!(joiner.steward.config().sn_max, STEWARD_SN_MAX);

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    let alice = b"alice".to_vec();
    let bob = b"bob".to_vec();
    let steward_list = steward_handle.steward.current_list().unwrap();
    let sync = ConversationSync {
        steward_members: steward_list.members().to_vec(),
        election_epoch: steward_list.election_epoch(),
        sn_min: steward_list.config().sn_min as u32,
        sn_max: steward_list.config().sn_max as u32,
        allow_subset_candidates: steward_handle.steward.config().allow_subset_candidates,
        peer_scores: vec![
            PeerScore {
                member_id: alice.clone(),
                score: STEWARD_THRESHOLD - 10,
            },
            PeerScore {
                member_id: bob.clone(),
                score: STEWARD_THRESHOLD + 10,
            },
        ],
        timing: None,
        retry_round: steward_list.retry_round(),
        max_reelection_attempts: steward_handle.steward.max_retries(),
        liveness_criteria_yes: steward_handle.liveness_criteria_yes,
        threshold_peer_score: STEWARD_THRESHOLD,
        pending_update_max_epochs: steward_handle.pending_update_max_epochs,
    };
    let app_msg: AppMessage = sync.clone().into();
    let sync_packet = steward_handle
        .mls
        .build_message(&app_msg, b"test-app-id")
        .unwrap();

    let result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_ref(),
        &sync_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    let received = match result {
        ProcessResult::ConversationSyncReceived(s) => s,
        other => panic!("Expected GroupSyncReceived, got {:?}", other),
    };

    assert_eq!(received.threshold_peer_score, STEWARD_THRESHOLD);
    assert_eq!(received.liveness_criteria_yes, STEWARD_LIVENESS_YES);
    assert_eq!(
        received.pending_update_max_epochs,
        STEWARD_PENDING_MAX_EPOCHS
    );
    assert_eq!(received.sn_min as usize, STEWARD_SN_MIN);
    assert_eq!(received.sn_max as usize, STEWARD_SN_MAX);

    let mut applied_protocol =
        StewardListConfig::new(received.sn_min as usize, received.sn_max as usize).unwrap();
    applied_protocol.allow_subset_candidates = received.allow_subset_candidates;
    joiner.steward.set_config(applied_protocol);
    joiner.liveness_criteria_yes = received.liveness_criteria_yes;
    joiner.pending_update_max_epochs = received.pending_update_max_epochs;

    assert_eq!(joiner.liveness_criteria_yes, STEWARD_LIVENESS_YES);
    assert_eq!(joiner.pending_update_max_epochs, STEWARD_PENDING_MAX_EPOCHS);
    assert_eq!(joiner.steward.config().sn_min, STEWARD_SN_MIN);
    assert_eq!(joiner.steward.config().sn_max, STEWARD_SN_MAX);

    let mut scoring = PeerScoringService::new(
        InMemoryPeerScoreStorage::new(),
        de_mls::app::FixedScoringProvider::with_default_deltas(),
        ScoringConfig {
            default_score: 100,
            threshold: 0,
        },
    );
    scoring.set_threshold(received.threshold_peer_score);
    let _ = scoring.apply_snapshot(&ScoreSnapshot {
        diverged: received
            .peer_scores
            .iter()
            .map(|ps| (ps.member_id.clone(), ps.score))
            .collect(),
    });
    let _ = scoring.apply_op(&ScoreOp {
        member_id: alice.clone(),
        event: ScoreEvent::SuccessfulCommit,
    });
    let below = scoring.members_below_threshold();
    assert!(
        below.contains(&alice),
        "alice (score {}) is below the synced threshold {STEWARD_THRESHOLD}",
        STEWARD_THRESHOLD - 10 + 10,
    );
    assert!(
        !below.contains(&bob),
        "bob (score {}) is above the synced threshold {STEWARD_THRESHOLD}",
        STEWARD_THRESHOLD + 10,
    );
}

#[test]
fn test_group_sync_idempotent_for_existing_members() {
    use de_mls::protos::de_mls::messages::v1::ConversationSync;

    let conversation_name = "sync-idempotent-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let mut steward_handle = setup_steward(conversation_name, steward_hex);
    let mut joiner = setup_joiner(conversation_name, joiner_hex);

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    // Manually give the joiner a steward list (simulating a previous sync)
    let members = joiner.mls.as_ref().unwrap().members().unwrap();
    assert!(joiner.steward.install_list(0, &members, 1, 0).is_ok());
    assert!(joiner.steward.current_list().is_some());

    let steward_list = steward_handle.steward.current_list().unwrap();
    let sync = ConversationSync {
        steward_members: steward_list.members().to_vec(),
        election_epoch: steward_list.election_epoch(),
        sn_min: steward_list.config().sn_min as u32,
        sn_max: steward_list.config().sn_max as u32,
        allow_subset_candidates: false,
        peer_scores: vec![],
        timing: None,
        retry_round: 0,
        max_reelection_attempts: 1,
        liveness_criteria_yes: true,
        threshold_peer_score: 0,
        pending_update_max_epochs: 3,
    };
    let app_msg: AppMessage = sync.into();
    let sync_packet = steward_handle
        .mls
        .build_message(&app_msg, b"test-app-id")
        .unwrap();

    // Joiner processes sync — core still surfaces GroupSyncReceived
    let result = process_inbound_compat(
        &mut joiner.group,
        joiner.mls.as_ref(),
        &sync_packet.payload,
        APP_MSG_SUBTOPIC,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::ConversationSyncReceived(_)),
        "Core should still return GroupSyncReceived"
    );

    // App layer would skip applying because the list is already set; verify
    // the existing list wasn't overwritten.
    assert_eq!(
        joiner.steward.current_list().unwrap().len(),
        1,
        "Existing list should be preserved (1 member), not overwritten by sync"
    );
}

#[test]
fn test_group_sync_carries_list_retry_round_not_group_counter() {
    let conversation_name = "sync-retry-tag-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let extra1_hex = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
    let extra2_hex = "0x90F79bf6EB2c4f870365E785982E1f101E93b906";

    let config = StewardListConfig::new(2, 4).unwrap();
    let mut steward_handle =
        setup_steward_with_config(conversation_name, steward_hex, config.clone());

    for hex in [joiner_hex, extra1_hex, extra2_hex] {
        let joiner = setup_joiner_with_config(conversation_name, hex, config.clone());
        steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    }
    let members = steward_handle.mls.members().unwrap();
    assert_eq!(members.len(), 4);

    let epoch = steward_handle.mls.current_epoch().unwrap();
    let accepted_round: u32 = 2;
    steward_handle
        .steward
        .install_list(epoch, &members, 4, accepted_round)
        .unwrap();
    steward_handle.steward.reset_retry();

    let list = steward_handle
        .steward
        .current_list()
        .expect("list set above");
    assert_eq!(list.retry_round(), accepted_round, "list keeps its seed");
    assert_eq!(
        steward_handle.steward.retry_round(),
        0,
        "counter was reset on accept — distinct from the list tag"
    );

    let round0 = StewardList::generate(
        epoch,
        conversation_name.as_bytes(),
        &members,
        4,
        config.clone(),
        0,
    )
    .unwrap();
    assert_ne!(
        list.members(),
        round0.members(),
        "test assumption: retry_round must shuffle the ordering"
    );

    use de_mls::protos::de_mls::messages::v1::ConversationSync;
    let sync = ConversationSync {
        steward_members: list.members().to_vec(),
        election_epoch: list.election_epoch(),
        sn_min: list.config().sn_min as u32,
        sn_max: list.config().sn_max as u32,
        allow_subset_candidates: false,
        peer_scores: vec![],
        timing: None,
        retry_round: list.retry_round(),
        max_reelection_attempts: steward_handle.steward.max_retries(),
        liveness_criteria_yes: true,
        threshold_peer_score: 0,
        pending_update_max_epochs: 3,
    };

    assert!(
        StewardList::validate(
            &sync.steward_members,
            sync.election_epoch,
            conversation_name.as_bytes(),
            &sync.steward_members,
            &config,
            sync.retry_round,
        )
        .unwrap(),
        "joiner validates when the wire retry_round matches the list's seed"
    );

    assert!(
        !StewardList::validate(
            &sync.steward_members,
            sync.election_epoch,
            conversation_name.as_bytes(),
            &sync.steward_members,
            &config,
            steward_handle.steward.retry_round(),
        )
        .unwrap(),
        "the post-accept counter value (0) regenerates a different ordering"
    );
}
