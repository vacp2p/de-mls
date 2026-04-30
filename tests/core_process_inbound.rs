//! Integration tests for `process_inbound`.

use prost::Message;

use de_mls::core::{
    CoreError, FreezeOutcome, ProcessResult, ProtocolConfig, StewardList, build_message,
    create_commit_candidate, finalize_freeze_round, group_members, prepare_to_join,
    process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use de_mls::mls_crypto::parse_wallet_address;
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, ConversationMessage, GroupUpdateRequest, app_message,
};

mod common;
use common::{
    default_steward_config, setup_joiner, setup_joiner_with_config, setup_mls, setup_steward,
    setup_steward_with_config, steward_add_joiner,
};

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

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);

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
fn test_process_inbound_welcome_non_steward_buffers_key_package() {
    let group_name = "non-steward-kp";

    let mls = setup_mls("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let mut group = prepare_to_join(group_name, mls.wallet_bytes(), default_steward_config());

    let (_joiner_mls, _joiner_handle, kp_packet) =
        setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

    let result = process_inbound(&mut group, &kp_packet.payload, WELCOME_SUBTOPIC, &mls).unwrap();

    // Every member (steward or not) now surfaces KPs so the app layer can
    // buffer them; promotion to a voting proposal is the app's decision.
    assert!(
        matches!(result, ProcessResult::MembershipChangeReceived(_)),
        "Expected MembershipChangeReceived, got {:?}",
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

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);

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

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
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
    let (welcome_packet2, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp2_packet);

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

    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
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
    let matched = matches!(
        &finalize.outcome,
        FreezeOutcome::Applied { result, .. } if matches!(**result, ProcessResult::LeaveGroup)
    );
    assert!(
        matched,
        "Expected LeaveGroup after finalize, got {finalize:?}"
    );
}

#[test]
fn test_process_inbound_raw_commit_payload_is_ignored() {
    let group_name = "raw-commit-ignored";

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
            .generate_and_set_steward_list(epoch, &members_2, sn, 0)
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

// ─────────────────────────── Group sync tests ───────────────────────────

/// Steward sends a GroupSync app message after adding a joiner.
/// The joiner receives it, validates it, and applies the steward list.
#[test]
fn test_group_sync_roundtrip() {
    use de_mls::protos::de_mls::messages::v1::GroupSync;

    let group_name = "sync-list-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let (steward_mls, mut steward_handle) = setup_steward(group_name, steward_hex);
    let (joiner_mls, mut joiner_handle, kp_packet) = setup_joiner(group_name, joiner_hex);

    // Steward adds joiner
    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);

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
    let sync = GroupSync {
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

    // Should get GroupSyncReceived
    match &result {
        ProcessResult::GroupSyncReceived(received_sync) => {
            assert_eq!(received_sync.steward_members, sync.steward_members);
            assert_eq!(received_sync.election_epoch, sync.election_epoch);
            assert_eq!(received_sync.sn_min, sync.sn_min);
            assert_eq!(received_sync.sn_max, sync.sn_max);
        }
        other => panic!("Expected GroupSyncReceived, got {:?}", other),
    }

    // Simulate app-layer handling: validate and apply
    if let ProcessResult::GroupSyncReceived(sync) = result {
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
                sync.election_epoch,
                group_name.as_bytes(),
                &sync.steward_members,
                &config,
                sync.retry_round,
            )
            .is_ok(),
            "Received steward list ordering should be valid"
        );

        let sn = sync.steward_members.len();
        assert!(
            joiner_handle
                .generate_and_set_steward_list(
                    sync.election_epoch,
                    &sync.steward_members,
                    sn,
                    sync.retry_round,
                )
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
    assert_eq!(joiner_list.election_epoch(), steward_list.election_epoch());
}

/// If a member already has a steward list, receiving a sync message is a no-op.
#[test]
fn test_group_sync_idempotent_for_existing_members() {
    use de_mls::protos::de_mls::messages::v1::GroupSync;

    let group_name = "sync-idempotent-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";

    let (steward_mls, mut steward_handle) = setup_steward(group_name, steward_hex);
    let (joiner_mls, mut joiner_handle, kp_packet) = setup_joiner(group_name, joiner_hex);

    // Steward adds joiner
    let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);

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
            .generate_and_set_steward_list(0, &members, 1, 0)
            .is_ok()
    );
    assert!(joiner_handle.steward_list().is_some());

    // Now steward sends a sync message
    let steward_list = steward_handle.steward_list().unwrap();
    let sync = GroupSync {
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
    };
    let app_msg: AppMessage = sync.into();
    let sync_packet =
        build_message(&steward_handle, &steward_mls, &app_msg, b"test-app-id").unwrap();

    // Joiner processes sync — should get GroupSyncReceived at the core level
    let result = process_inbound(
        &mut joiner_handle,
        &sync_packet.payload,
        APP_MSG_SUBTOPIC,
        &joiner_mls,
    )
    .unwrap();
    assert!(
        matches!(result, ProcessResult::GroupSyncReceived(_)),
        "Core should still return GroupSyncReceived"
    );

    // But the app layer would skip it because the list is already set.
    // We verify the existing list wasn't overwritten (still 1 member, not steward's list).
    assert_eq!(
        joiner_handle.steward_list().unwrap().len(),
        1,
        "Existing list should be preserved (1 member), not overwritten by sync"
    );
}

/// A list accepted at `retry_round > 0` must ship its generation seed
/// on `GroupSync` so the joiner re-derives the same ordering. The seed
/// lives on `StewardList` as a frozen tag, distinct from
/// `Group::reelection_round` (the dynamic counter, which resets to 0
/// on accept). Sourcing the wire `retry_round` from the list keeps the
/// two values from diverging after an accept.
#[test]
fn test_group_sync_carries_list_retry_round_not_group_counter() {
    let group_name = "sync-retry-tag-group";
    let steward_hex = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    let joiner_hex = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8";
    let extra1_hex = "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC";
    let extra2_hex = "0x90F79bf6EB2c4f870365E785982E1f101E93b906";

    // sn=4 over 4 members: retry_round affects ordering, not membership.
    let config = ProtocolConfig::new(2, 4).unwrap();
    let (steward_mls, mut steward_handle) =
        setup_steward_with_config(group_name, steward_hex, config.clone());

    // Pull three joiners into the MLS group so we have a four-member set
    // for a meaningful hash-sorted list.
    for hex in [joiner_hex, extra1_hex, extra2_hex] {
        let (_, _, kp) = setup_joiner_with_config(group_name, hex, config.clone());
        steward_add_joiner(&steward_mls, &mut steward_handle, &kp);
    }
    let members = group_members(&steward_handle, &steward_mls).unwrap();
    assert_eq!(members.len(), 4);

    // Simulate an election accepted after two rejections: the resulting
    // list is generated at retry_round=2 and stored with that tag.
    let epoch = steward_mls.current_epoch(group_name).unwrap();
    let accepted_round: u32 = 2;
    steward_handle
        .generate_and_set_steward_list(epoch, &members, 4, accepted_round)
        .unwrap();
    // Accept-side bookkeeping: the dynamic counter clears; the list's
    // seed is unaffected.
    steward_handle.reset_reelection_round();

    let list = steward_handle.steward_list().expect("list set above");
    assert_eq!(list.retry_round(), accepted_round, "list keeps its seed");
    assert_eq!(
        steward_handle.reelection_round(),
        0,
        "counter was reset on accept — distinct from the list tag"
    );

    // The round-2 ordering must differ from round 0 for the assertion
    // below to be meaningful — if they happened to match, the seed
    // wouldn't be load-bearing.
    let round0 =
        StewardList::generate(epoch, group_name.as_bytes(), &members, 4, config.clone(), 0)
            .unwrap();
    assert_ne!(
        list.members(),
        round0.members(),
        "test assumption: retry_round must shuffle the ordering"
    );

    // Build a GroupSync the way `send_group_sync` does — seed sourced
    // from the list's `retry_round`.
    use de_mls::protos::de_mls::messages::v1::GroupSync;
    let sync = GroupSync {
        steward_members: list.members().to_vec(),
        election_epoch: list.election_epoch(),
        sn_min: list.config().sn_min as u32,
        sn_max: list.config().sn_max as u32,
        allow_subset_candidates: false,
        peer_scores: vec![],
        timing: None,
        retry_round: list.retry_round(),
        max_reelection_attempts: steward_handle.max_reelection_attempts(),
        liveness_criteria_yes: true,
        threshold_peer_score: 0,
    };

    // Joiner re-derives the ordering using the wire seed — should match.
    assert!(
        StewardList::validate(
            &sync.steward_members,
            sync.election_epoch,
            group_name.as_bytes(),
            &sync.steward_members,
            &config,
            sync.retry_round,
        )
        .unwrap(),
        "joiner validates when the wire retry_round matches the list's seed"
    );

    // The counter's current value (0) regenerates a different ordering —
    // confirms `retry_round`-on-list is load-bearing, not redundant with
    // the counter.
    assert!(
        !StewardList::validate(
            &sync.steward_members,
            sync.election_epoch,
            group_name.as_bytes(),
            &sync.steward_members,
            &config,
            steward_handle.reelection_round(),
        )
        .unwrap(),
        "the post-accept counter value (0) regenerates a different ordering"
    );
}
