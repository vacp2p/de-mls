//! A removed member's session emits `Leaving` once the removal commit
//! merges on their side. The User-side cleanup path then runs
//! `finalize_self_leave`, which evicts the entry from the registry.

use std::time::Duration;

use de_mls::app::{CreatorVote, DispatchOutcome, SessionRunner};
use de_mls::core::{SessionEvent, StewardListConfig};
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
async fn removed_member_emits_leaving_and_is_evicted() {
    // sn_max = 2 → 2 of 3 members are stewards, exactly one isn't.
    // Target the non-steward so the commit isn't a self-remove (which
    // MLS rejects mid-freeze and would keep them stuck in Reelection).
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB, CHARLIE],
        "leave",
        fast_test_config(),
        StewardListConfig::new(1, 2).unwrap(),
    )
    .await;

    // Find the non-steward.
    let mut target_idx = None;
    for (i, (u, _)) in users.iter().enumerate() {
        let s = u.lookup_entry("leave").unwrap().unwrap();
        if !s.read().unwrap().is_steward_for_self() {
            target_idx = Some(i);
            break;
        }
    }
    let target_idx = target_idx.expect("sn_max=2 with 3 members yields exactly one non-steward");
    // The first steward we find drives the proposal.
    let steward_idx = (0..users.len())
        .find(|i| *i != target_idx)
        .expect("at least one steward must exist");

    let steward_session = users[steward_idx].0.lookup_entry("leave").unwrap().unwrap();
    let target_session = users[target_idx].0.lookup_entry("leave").unwrap().unwrap();

    let target_id = parse_wallet_to_bytes(&users[target_idx].0.identity_string()).unwrap();
    let request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember {
                identity: target_id,
            },
        )),
    };
    SessionRunner::initiate_proposal(&steward_session, request, CreatorVote::Yes)
        .await
        .unwrap();

    // Drive packet relay + polling until the target is evicted from its
    // User registry. Mirrors the gateway's polling loop: when
    // `poll_freeze_status` returns `DispatchOutcome::LeaveRequested`, the
    // caller is responsible for running `User::finalize_self_leave`.
    let mut target_evicted = false;
    for _ in 0..30 {
        settle_for(Duration::from_millis(40)).await;
        for (i, (u, _)) in users.iter().enumerate() {
            if let Some(s) = u.lookup_entry("leave").unwrap() {
                let _ = SessionRunner::tick_deadlines(&s).await;
                if i == target_idx
                    && matches!(
                        SessionRunner::poll_freeze_status(&s).await,
                        Ok((_, DispatchOutcome::LeaveRequested))
                    )
                {
                    u.finalize_self_leave("leave").await.unwrap();
                } else {
                    let _ = SessionRunner::poll_freeze_status(&s).await;
                    let _ = SessionRunner::check_member_freeze(&s).await;
                }
            }
        }
        let mut packets = Vec::new();
        for (_, h) in &users {
            packets.extend(h.lock().unwrap().drain_packets());
        }
        for p in &packets {
            for (u, _) in &users {
                let _ = u.process_inbound_packet(to_inbound(p)).await;
            }
        }
        if users[target_idx].0.lookup_entry("leave").unwrap().is_none() {
            target_evicted = true;
            break;
        }
    }
    assert!(
        target_evicted,
        "removed member must be evicted from their own registry"
    );

    let target_events = target_session.read().unwrap().drain_events();
    assert!(
        target_events
            .iter()
            .any(|e| matches!(e, SessionEvent::Leaving)),
        "removed member's session must emit Leaving"
    );
}
