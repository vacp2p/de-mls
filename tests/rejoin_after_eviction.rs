//! Evicted member can rejoin the same conversation via a fresh
//! `start_conversation(.., is_creation = false)` flow.

use std::time::Duration;

use de_mls::app::{CreatorVote, DispatchOutcome, SessionRunner};
use de_mls::core::{ConversationState, StewardListConfig};
use de_mls::identity::parse_wallet_to_bytes;
use de_mls::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, RemoveMember, conversation_update_request,
};

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, fast_test_config, settle_for, to_inbound,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

#[tokio::test]
async fn evicted_member_can_rejoin_at_higher_epoch() {
    // sn_max = 2 → 2 of 3 are stewards; target the non-steward so the
    // removal commit isn't a self-remove.
    let mut users = bootstrap_joined_conversation(
        &[ALICE, BOB, CHARLIE],
        "rejoin",
        fast_test_config(),
        StewardListConfig::new(1, 2).unwrap(),
    )
    .await;

    let mut target_idx = None;
    for (i, (u, _)) in users.iter().enumerate() {
        let s = u.lookup_entry("rejoin").unwrap().unwrap();
        if !s.read().unwrap().is_steward_for_self() {
            target_idx = Some(i);
            break;
        }
    }
    let target_idx = target_idx.expect("sn_max=2 with 3 members yields exactly one non-steward");
    let steward_idx = (0..users.len())
        .find(|i| *i != target_idx)
        .expect("at least one steward must exist");

    // Capture the pre-eviction epoch on the steward's side so we can
    // assert the rejoin lands at a strictly later one.
    let steward_session = users[steward_idx]
        .0
        .lookup_entry("rejoin")
        .unwrap()
        .unwrap();
    let pre_remove_epoch = steward_session
        .read()
        .unwrap()
        .get_epoch_and_retry()
        .unwrap()
        .0;

    // Phase 1: removal.
    let target_id = parse_wallet_to_bytes(&users[target_idx].0.identity_string()).unwrap();
    let request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember {
                identity: target_id.clone(),
            },
        )),
    };
    SessionRunner::initiate_proposal(&steward_session, request, CreatorVote::Yes).unwrap();

    let mut target_evicted = false;
    for _ in 0..30 {
        drive_one_round(&users, target_idx).await;
        if users[target_idx]
            .0
            .lookup_entry("rejoin")
            .unwrap()
            .is_none()
        {
            target_evicted = true;
            break;
        }
    }
    assert!(target_evicted, "target must be evicted from its registry");

    // Phase 2: target rejoins by registering as a joiner and shipping a
    // fresh KP. Drive the standard join cycle until they're Working
    // again.
    users[target_idx]
        .0
        .start_conversation("rejoin", false)
        .await
        .unwrap();

    let new_session = users[target_idx].0.lookup_entry("rejoin").unwrap().unwrap();
    let kp = users[target_idx].0.generate_key_package().unwrap();
    SessionRunner::send_kp_message(&new_session, kp)
        .await
        .unwrap();

    let mut rejoined = false;
    for _ in 0..30 {
        drive_one_round(&users, target_idx).await;
        let s = users[target_idx].0.lookup_entry("rejoin").unwrap().unwrap();
        if s.read().unwrap().get_conversation_state() == ConversationState::Working {
            rejoined = true;
            break;
        }
    }
    assert!(rejoined, "target must rejoin and reach Working state");

    let post_rejoin_epoch = users[target_idx]
        .0
        .lookup_entry("rejoin")
        .unwrap()
        .unwrap()
        .read()
        .unwrap()
        .get_epoch_and_retry()
        .unwrap()
        .0;
    assert!(
        post_rejoin_epoch > pre_remove_epoch,
        "rejoin must land at a strictly later epoch ({post_rejoin_epoch} > {pre_remove_epoch})"
    );

    // Steward sees the rejoined identity back in the member set.
    let steward_members = steward_session
        .read()
        .unwrap()
        .get_conversation_members()
        .unwrap();
    let target_display = users[target_idx].0.identity_string().to_lowercase();
    assert!(
        steward_members
            .iter()
            .any(|m| m.to_lowercase() == target_display),
        "steward must see the rejoined identity in its member list, got {steward_members:?}"
    );
}

async fn drive_one_round(
    users: &[(
        common::session_fixtures::TestUser,
        std::sync::Arc<common::session_fixtures::CapturingTransport>,
    )],
    target_idx: usize,
) {
    settle_for(Duration::from_millis(40)).await;
    for (i, (u, _)) in users.iter().enumerate() {
        if let Some(s) = u.lookup_entry("rejoin").unwrap() {
            let pfs = SessionRunner::poll_freeze_status(&s).await;
            if i == target_idx && matches!(pfs, Ok((_, DispatchOutcome::LeaveRequested))) {
                u.finalize_self_leave("rejoin").await.unwrap();
                continue;
            }
            let _ = SessionRunner::check_member_freeze(&s).await;
            let _ = SessionRunner::check_pending_join(&s).unwrap();
        }
    }
    let mut packets = Vec::new();
    for (_, h) in users {
        packets.extend(h.drain_packets());
    }
    for p in &packets {
        for (u, _) in users {
            let _ = u.process_inbound_packet(to_inbound(p)).await;
        }
    }
}
