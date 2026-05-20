//! Application message roundtrip through `SessionRunner::send_app_message`
//! and `SessionRunner::dispatch_inbound_result` → `SessionEvent::AppMessage`.

use std::time::Duration;

use de_mls::app::SessionRunner;
use de_mls::core::{SessionEvent, StewardListConfig};
use de_mls::protos::de_mls::messages::v1::app_message;
use tokio::sync::broadcast;

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, fast_test_config, settle_for, to_inbound,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[tokio::test]
async fn chat_message_delivered_to_peer_as_app_message_event() {
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "chat",
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    )
    .await;

    let alice_session = users[0].0.lookup_entry("chat").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("chat").unwrap().unwrap();

    let mut bob_events = bob_session.read().unwrap().subscribe();

    SessionRunner::send_app_message(&alice_session, b"Hello from alice".to_vec())
        .await
        .unwrap();

    // Relay alice's outbound to bob.
    settle_for(Duration::from_millis(40)).await;
    let packets = users[0].1.lock().unwrap().drain_packets();
    for p in packets {
        let _ = users[1].0.process_inbound_packet(to_inbound(&p)).await;
    }

    let chat = find_chat_message(&mut bob_events);
    let (body, sender) = chat.expect("bob must surface alice's chat message as a SessionEvent");
    assert_eq!(body, b"Hello from alice");
    assert_eq!(sender, users[0].0.identity_string());
}

/// Drain events, find the first `ConversationMessage` payload and return
/// its (body, sender).
fn find_chat_message(rx: &mut broadcast::Receiver<SessionEvent>) -> Option<(Vec<u8>, String)> {
    use tokio::sync::broadcast::error::TryRecvError;
    loop {
        match rx.try_recv() {
            Ok(SessionEvent::AppMessage(msg)) => {
                if let Some(app_message::Payload::ConversationMessage(cm)) = msg.payload {
                    return Some((cm.message, cm.sender));
                }
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => return None,
            Err(TryRecvError::Lagged(_)) => continue,
        }
    }
}
