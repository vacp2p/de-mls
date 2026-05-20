//! Two narrow checks on `core::process_inbound`: a second welcome for a
//! different KP is ignored, and a `ConversationSync` packet propagates
//! divergent per-conversation config (timing, threshold, scoring) to a
//! joiner that hasn't yet installed a steward list.

use de_mls::core::{ProcessResult, StewardListConfig, StewardListPlugin};
use de_mls::ds::APP_MSG_SUBTOPIC;
use de_mls::mls_crypto::MlsService;
use de_mls::protos::de_mls::messages::v1::AppMessage;

mod common;
use common::{
    default_steward_list_config, process_inbound_compat, setup_joiner, setup_joiner_with_config,
    setup_steward, setup_steward_with_config, steward_add_joiner,
};

// ─────────────────────────── process_inbound tests ───────────────────────────

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
    // MLS state. `SessionRunner` additionally guards on
    // `handle.mls().is_some()` to skip wholesale; both safeguards land at
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

// ─────────────────────────── Conversation sync tests ───────────────────────────

#[test]
fn test_conversation_sync_propagates_divergent_per_conv_config() {
    use de_mls::core::{PeerScoringPlugin, PeerScoringService, ScoreSnapshot};
    use de_mls::core::{ScoreEvent, ScoreOp, ScoringConfig};
    use de_mls::defaults::InMemoryPeerScoreStorage;
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
        setup_joiner_with_config(conversation_name, joiner_hex, default_steward_list_config());

    steward_handle.liveness_criteria_yes = STEWARD_LIVENESS_YES;
    steward_handle.pending_update_max_epochs = STEWARD_PENDING_MAX_EPOCHS;

    assert_ne!(joiner.liveness_criteria_yes, STEWARD_LIVENESS_YES);
    assert_ne!(joiner.pending_update_max_epochs, STEWARD_PENDING_MAX_EPOCHS);
    assert_ne!(joiner.steward_list.config().sn_min, STEWARD_SN_MIN);
    assert_ne!(joiner.steward_list.config().sn_max, STEWARD_SN_MAX);

    let (welcome_packet, _) = steward_add_joiner(&mut steward_handle, &joiner.kp_packet);
    joiner.accept_welcome_packet(&welcome_packet);

    let alice = b"alice".to_vec();
    let bob = b"bob".to_vec();
    let steward_list = steward_handle.steward_list.current_list().unwrap();
    let sync = ConversationSync {
        steward_members: steward_list.members().to_vec(),
        election_epoch: steward_list.election_epoch(),
        sn_min: steward_list.config().sn_min as u32,
        sn_max: steward_list.config().sn_max as u32,
        allow_subset_candidates: steward_handle.steward_list.config().allow_subset_candidates,
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
        max_reelection_attempts: steward_handle.steward_list.max_retries(),
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
        other => panic!("Expected ConversationSyncReceived, got {:?}", other),
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
    joiner.steward_list.set_config(applied_protocol);
    joiner.liveness_criteria_yes = received.liveness_criteria_yes;
    joiner.pending_update_max_epochs = received.pending_update_max_epochs;

    assert_eq!(joiner.liveness_criteria_yes, STEWARD_LIVENESS_YES);
    assert_eq!(joiner.pending_update_max_epochs, STEWARD_PENDING_MAX_EPOCHS);
    assert_eq!(joiner.steward_list.config().sn_min, STEWARD_SN_MIN);
    assert_eq!(joiner.steward_list.config().sn_max, STEWARD_SN_MAX);

    let mut scoring = PeerScoringService::new(
        InMemoryPeerScoreStorage::new(),
        de_mls::core::default_score_deltas(),
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
