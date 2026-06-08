//! Freeze coordinator event ordering.
//!
//! During a commit cycle, every session must emit phase events in the
//! order: `Freezing → Selection → CommitApplied → Working`. The
//! coordinator runs in `SessionRunner` (Freeze is not a plug-in), so this
//! is a direct probe of its orchestration.

use std::time::Duration;

use de_mls::app::CreatorVote;
use de_mls::core::{ConversationState, SessionEvent, StewardListConfig};
use de_mls::member_id::MemberId;
use de_mls::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, RemoveMember, conversation_update_request,
};

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, fast_test_config, flush_session, poll_once, settle_for,
    to_inbound,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[test]
fn freeze_cycle_emits_phase_events_in_order() {
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "c3",
        fast_test_config(),
        StewardListConfig::new(1, 5).unwrap(),
    );

    let alice_session = users[0].0.lookup_entry("c3").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("c3").unwrap().unwrap();
    let alice_tx = users[0].1.clone();
    let bob_tx = users[1].1.clone();

    // We only assert on the steward (alice); the target's (bob's) event
    // stream is complicated by the LeaveConversation path and is covered
    // separately in the self-leave tests.
    //
    // Drain whatever bootstrap emitted (e.g. the join-cycle's
    // `CommitApplied(MemberInvite(bob))`) so the loop below only collects
    // events fired by the RemoveMember proposal we're about to file.
    let _ = alice_session.read().unwrap().drain_events();

    let bob_id = common::WalletMemberId::from_hex(&users[1].0.member_id_string())
        .member_id_bytes()
        .to_vec();
    let remove_request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember {
                member_id: bob_id.clone(),
            },
        )),
    };
    alice_session
        .write()
        .unwrap()
        .initiate_proposal(remove_request, CreatorVote::Yes)
        .unwrap();

    // Drive polling + packet relay until both sessions are back in
    // Working (bob will actually exit the conversation, so we check
    // alice's Working state plus a quiet-period after.)
    let mut alice_phases: Vec<PhaseEntry> = Vec::new();
    let mut saw_freezing = false;
    for _ in 0..40 {
        settle_for(Duration::from_millis(40));
        poll_once(&alice_session);
        poll_once(&bob_session);
        flush_session(&alice_session, &alice_tx);
        flush_session(&bob_session, &bob_tx);

        let mut packets = Vec::new();
        packets.extend(alice_tx.lock().unwrap().drain_packets());
        packets.extend(bob_tx.lock().unwrap().drain_packets());
        for p in &packets {
            let _ = users[0].0.process_inbound_packet(to_inbound(p));
            let _ = users[1].0.process_inbound_packet(to_inbound(p));
        }

        alice_phases.extend(drain_phase_log(&alice_session));
        let alice_state = alice_session.read().unwrap().get_conversation_state();
        if alice_state == ConversationState::Freezing || alice_state == ConversationState::Selection
        {
            saw_freezing = true;
        }
        if saw_freezing && alice_state == ConversationState::Working && packets.is_empty() {
            // Pump one more round to catch trailing events.
            settle_for(Duration::from_millis(40));
            poll_once(&alice_session);
            alice_phases.extend(drain_phase_log(&alice_session));
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
            Some(conversation_update_request::Payload::RemoveMember(rm)) if rm.member_id == bob_id
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

/// Drain pending session events and project to the phase-relevant subset.
fn drain_phase_log(session: &common::session_fixtures::SessionArc) -> Vec<PhaseEntry> {
    session
        .read()
        .unwrap()
        .drain_events()
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::PhaseChange(ConversationState::Freezing) => Some(PhaseEntry::Freezing),
            SessionEvent::PhaseChange(ConversationState::Selection) => Some(PhaseEntry::Selection),
            SessionEvent::PhaseChange(ConversationState::Working) => Some(PhaseEntry::Working),
            SessionEvent::CommitApplied(batch) => Some(PhaseEntry::CommitApplied(batch)),
            _ => None,
        })
        .collect()
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
