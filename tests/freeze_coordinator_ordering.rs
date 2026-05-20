//! Freeze coordinator event ordering.
//!
//! During a commit cycle, every session must emit phase events in the
//! order: `Freezing → Selection → CommitApplied → Working`. The
//! coordinator runs in `SessionRunner` (Freeze is not a plug-in), so this
//! is a direct probe of its orchestration.

use std::time::Duration;

use de_mls::app::{CreatorVote, SessionRunner};
use de_mls::core::{ConversationState, SessionEvent, StewardListConfig};
use de_mls::identity::parse_wallet_to_bytes;
use de_mls::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, RemoveMember, conversation_update_request,
};
use tokio::sync::broadcast;

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, fast_test_config, poll_once, settle_for, to_inbound,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[tokio::test]
async fn freeze_cycle_emits_phase_events_in_order() {
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "c3",
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    )
    .await;

    let alice_session = users[0].0.lookup_entry("c3").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("c3").unwrap().unwrap();
    let alice_tx = users[0].1.clone();
    let bob_tx = users[1].1.clone();

    // Subscribe BEFORE filing the proposal so we don't miss the early
    // Freezing event. We only assert on the steward (alice); the target's
    // (bob's) event stream is complicated by the LeaveConversation path
    // and is covered separately in the self-leave tests.
    let mut alice_events = alice_session.read().await.subscribe();

    let bob_id = parse_wallet_to_bytes(&users[1].0.identity_string()).unwrap();
    let remove_request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember {
                identity: bob_id.clone(),
            },
        )),
    };
    SessionRunner::initiate_proposal(&alice_session, remove_request, CreatorVote::Yes)
        .await
        .unwrap();

    // Drive polling + packet relay until both sessions are back in
    // Working (bob will actually exit the conversation, so we check
    // alice's Working state plus a quiet-period after.)
    let mut alice_phases: Vec<PhaseEntry> = Vec::new();
    let mut saw_freezing = false;
    for _ in 0..40 {
        settle_for(Duration::from_millis(40)).await;
        poll_once(&alice_session).await;
        poll_once(&bob_session).await;

        let mut packets = Vec::new();
        packets.extend(alice_tx.drain_packets());
        packets.extend(bob_tx.drain_packets());
        for p in &packets {
            let _ = users[0].0.process_inbound_packet(to_inbound(p)).await;
            let _ = users[1].0.process_inbound_packet(to_inbound(p)).await;
        }

        alice_phases.extend(drain_phase_log(&mut alice_events));
        let alice_state = alice_session.read().await.get_conversation_state();
        if alice_state == ConversationState::Freezing || alice_state == ConversationState::Selection
        {
            saw_freezing = true;
        }
        if saw_freezing && alice_state == ConversationState::Working && packets.is_empty() {
            // Pump one more round to catch trailing events.
            settle_for(Duration::from_millis(40)).await;
            poll_once(&alice_session).await;
            alice_phases.extend(drain_phase_log(&mut alice_events));
            break;
        }
    }

    assert_subsequence_matches(
        &alice_phases,
        &[
            PhaseTag::Freezing,
            PhaseTag::Selection,
            PhaseTag::CommitApplied,
            PhaseTag::Working,
        ],
        "alice (steward) must emit Freezing → Selection → CommitApplied → Working",
    );

    // Cross-check: the CommitApplied payload on alice's side names bob.
    let alice_commit = alice_phases
        .iter()
        .find_map(|p| match p {
            PhaseEntry::CommitApplied(batch) => Some(batch.clone()),
            _ => None,
        })
        .expect("alice must emit CommitApplied at least once");
    let removes_bob = alice_commit.iter().any(|req| {
        matches!(
            req.payload.as_ref(),
            Some(conversation_update_request::Payload::RemoveMember(rm)) if rm.identity == bob_id
        )
    });
    assert!(
        removes_bob,
        "CommitApplied batch must contain RemoveMember(bob)"
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PhaseTag {
    Freezing,
    Selection,
    CommitApplied,
    Working,
}

#[derive(Debug, Clone)]
enum PhaseEntry {
    Freezing,
    Selection,
    CommitApplied(Vec<ConversationUpdateRequest>),
    Working,
}

impl PhaseEntry {
    fn tag(&self) -> PhaseTag {
        match self {
            PhaseEntry::Freezing => PhaseTag::Freezing,
            PhaseEntry::Selection => PhaseTag::Selection,
            PhaseEntry::CommitApplied(_) => PhaseTag::CommitApplied,
            PhaseEntry::Working => PhaseTag::Working,
        }
    }
}

/// Drain pending events from a broadcast receiver and project to the
/// phase-relevant subset. Tolerates `Lagged` (which means events have
/// overflowed the channel buffer between polls) but stops on `Empty` /
/// `Closed`.
fn drain_phase_log(rx: &mut broadcast::Receiver<SessionEvent>) -> Vec<PhaseEntry> {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut out = Vec::new();
    let mut lagged = 0u64;
    loop {
        match rx.try_recv() {
            Ok(SessionEvent::PhaseChange(ConversationState::Freezing)) => {
                out.push(PhaseEntry::Freezing)
            }
            Ok(SessionEvent::PhaseChange(ConversationState::Selection)) => {
                out.push(PhaseEntry::Selection)
            }
            Ok(SessionEvent::PhaseChange(ConversationState::Working)) => {
                out.push(PhaseEntry::Working)
            }
            Ok(SessionEvent::PhaseChange(_)) => {}
            Ok(SessionEvent::CommitApplied(batch)) => out.push(PhaseEntry::CommitApplied(batch)),
            Ok(_) => {}
            Err(TryRecvError::Lagged(n)) => {
                lagged += n;
                continue;
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => break,
        }
    }
    if lagged > 0 {
        eprintln!("[drain_phase_log] lagged: dropped {lagged} events");
    }
    out
}

/// Assert that `phases` contains every element of `expected` in order
/// (extra intervening entries are fine).
fn assert_subsequence_matches(phases: &[PhaseEntry], expected: &[PhaseTag], msg: &str) {
    let actual: Vec<PhaseTag> = phases.iter().map(PhaseEntry::tag).collect();
    let mut iter = actual.iter();
    for want in expected {
        let found = iter.any(|got| got == want);
        assert!(
            found,
            "{msg}\n  expected subsequence: {expected:?}\n  actual events: {actual:?}"
        );
    }
}
