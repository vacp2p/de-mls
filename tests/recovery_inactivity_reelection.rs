//! Recovery inactivity → Reelection.
//!
//! When a non-steward member has approved work but their freeze window
//! elapses without a valid commit candidate (e.g. the elected steward
//! went silent), `finalize_freeze_round` returns `NoCandidate`. With
//! `has_proposals == true` that drives `start_reelection`, and the
//! `entered_reelection` branch of `poll_freeze_status` initiates a
//! recovery steward election.

use std::time::Duration;

use de_mls::app::{CreatorVote, SessionRunner};
use de_mls::core::{ConversationState, StewardListConfig};
use de_mls::member_id::MemberId;
use de_mls::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, RemoveMember, conversation_update_request,
};

mod common;
use common::session_fixtures::{
    bootstrap_joined_conversation, fast_test_config, poll_once, settle_for, to_inbound,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

#[test]
fn silent_steward_drives_observer_to_reelection() {
    // `sn_max = 1` → exactly one steward at any epoch. The non-steward
    // member is the "observer" that exercises the Reelection path.
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "b2",
        fast_test_config(),
        StewardListConfig::new(1, 1).unwrap(),
    );

    let alice_session = users[0].0.lookup_entry("b2").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("b2").unwrap().unwrap();

    let alice_is_steward = alice_session.read().unwrap().is_steward_for_self();
    let bob_is_steward = bob_session.read().unwrap().is_steward_for_self();
    assert!(
        alice_is_steward ^ bob_is_steward,
        "sn_max=1 must yield exactly one steward (alice_is_steward={alice_is_steward}, bob_is_steward={bob_is_steward})"
    );

    let (steward_idx, observer_idx) = if alice_is_steward { (0, 1) } else { (1, 0) };
    let observer_session = users[observer_idx].0.lookup_entry("b2").unwrap().unwrap();
    let steward_tx = users[steward_idx].1.clone();
    let observer_tx = users[observer_idx].1.clone();

    // Observer files a proposal. With `CreatorVote::Yes` and `expected_voters=2`,
    // the steward's auto-vote completes consensus. The exact proposal payload
    // doesn't matter — we just need approved work in the queue.
    let steward_member_id =
        common::WalletMemberId::from_hex(&users[steward_idx].0.member_id_string())
            .member_id_bytes()
            .to_vec();
    let request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember {
                member_id: steward_member_id,
            },
        )),
    };
    SessionRunner::initiate_proposal(&observer_session, request, CreatorVote::Yes).unwrap();

    // Phase 1: pump packets normally until both sides have approved=1.
    let mut consensus_reached = false;
    for _ in 0..20 {
        settle_for(Duration::from_millis(40));
        poll_once(&alice_session);
        poll_once(&bob_session);
        let packets = users[0].1.lock().unwrap().drain_packets();
        for p in packets {
            let _ = users[1].0.process_inbound_packet(to_inbound(&p));
        }
        let packets = users[1].1.lock().unwrap().drain_packets();
        for p in packets {
            let _ = users[0].0.process_inbound_packet(to_inbound(&p));
        }
        let observer_approved = observer_session
            .read()
            .unwrap()
            .get_approved_proposals_for_current_epoch()
            .len();
        if observer_approved > 0 {
            consensus_reached = true;
            break;
        }
    }
    assert!(consensus_reached, "consensus must resolve before phase 2");

    // Phase 2: withhold the steward's outbound. The observer enters
    // Freezing, can't create its own candidate (not a steward), doesn't
    // receive the steward's, freeze window elapses → NoCandidate →
    // start_reelection.
    let mut entered_reelection = false;
    for _ in 0..20 {
        settle_for(Duration::from_millis(40));
        poll_once(&observer_session);
        poll_once(&alice_session); // keep the steward polling, just discard its packets
        poll_once(&bob_session);

        // Discard everything the steward emits, deliver everything else.
        let _ = steward_tx.lock().unwrap().drain_packets();
        let packets = observer_tx.lock().unwrap().drain_packets();
        for p in packets {
            let _ = users[steward_idx].0.process_inbound_packet(to_inbound(&p));
        }

        let observer_state = observer_session.read().unwrap().get_conversation_state();
        if observer_state == ConversationState::Reelection {
            entered_reelection = true;
            break;
        }
    }
    assert!(
        entered_reelection,
        "observer must reach Reelection after the steward goes silent"
    );
}
