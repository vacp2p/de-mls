//! Steward-list rotation reconciles by group size.
//!
//! `members <= sn_max`: every member is a steward, so the list is the full
//! membership — each node regenerates it deterministically on commit-merge,
//! no consensus round. `members > sn_max`: the list is a genuine subset, so
//! peers run a voted election to agree which members serve.
//!
//! The small-group path is the fix for the doomed-election storm: a 2-member
//! group used to open a steward election that could never reach quorum, time
//! out, and escalate to a `Deadlock` ECP. These tests pin both branches.

use std::time::Duration;

use de_mls::app::MemberRole;
use de_mls::core::{ConversationState, StewardListConfig};
use de_mls::member_id::MemberId;

mod common;
use common::WalletMemberId;
use common::session_fixtures::{
    SessionArc, TestUser, TransportHandle, bootstrap_joined_conversation, fast_test_config,
    poll_once, settle_for, to_inbound,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

/// Sorted member ids carrying any steward role.
fn steward_set(roles: &[(Vec<u8>, MemberRole)]) -> Vec<Vec<u8>> {
    let mut ids: Vec<Vec<u8>> = roles
        .iter()
        .filter(|(_, role)| {
            matches!(
                role,
                MemberRole::EpochSteward | MemberRole::BackupSteward | MemberRole::Steward
            )
        })
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    ids
}

fn member_id(users: &[(TestUser, TransportHandle)], idx: usize) -> Vec<u8> {
    WalletMemberId::from_hex(&users[idx].0.member_id_string())
        .member_id_bytes()
        .to_vec()
}

/// Drain every transport and feed each packet to every user, mirroring the
/// gateway relay. Echo-dedup drops a user's own packets.
async fn relay_all(users: &[(TestUser, TransportHandle)]) {
    let mut packets = Vec::new();
    for (_, h) in users {
        packets.extend(h.lock().unwrap().drain_packets());
    }
    for p in &packets {
        for (u, _) in users {
            let _ = u.process_inbound_packet(to_inbound(p));
        }
    }
}

#[tokio::test]
async fn small_group_regenerates_locally_no_election() {
    // n = 2, sn_max = 3 → every member is a steward; the list is the full
    // membership, installed locally with no vote.
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "sg",
        fast_test_config(),
        StewardListConfig::new(1, 3).unwrap(),
    )
    .await;

    let alice_session = users[0].0.lookup_entry("sg").unwrap().unwrap();
    let bob_session = users[1].0.lookup_entry("sg").unwrap().unwrap();

    let mut both = vec![member_id(&users, 0), member_id(&users, 1)];
    both.sort();

    // Both nodes installed the identical full-membership list: every member
    // is a steward, no plain `Member`.
    let alice_roles = alice_session.read().unwrap().get_member_roles().unwrap();
    let bob_roles = bob_session.read().unwrap().get_member_roles().unwrap();
    assert_eq!(
        steward_set(&alice_roles),
        both,
        "alice's steward list must be the full membership"
    );
    assert_eq!(
        steward_set(&bob_roles),
        both,
        "bob's steward list must match alice's exactly"
    );

    // No election ever ran: a doomed steward election would bump retry_round
    // off zero and (after retries) escalate to a Deadlock ECP, flipping state
    // out of Working. Poll well past every inactivity/consensus deadline and
    // assert sustained stability.
    for _ in 0..20 {
        settle_for(Duration::from_millis(40)).await;
        for s in [&alice_session, &bob_session] {
            poll_once(s);
        }
        relay_all(&users).await;
    }

    for (label, s) in [("alice", &alice_session), ("bob", &bob_session)] {
        let (_, retry) = s.read().unwrap().get_epoch_and_retry().unwrap();
        assert_eq!(
            retry, 0,
            "{label} retry_round must stay 0 (no election fired)"
        );
        assert_eq!(
            s.read().unwrap().get_conversation_state(),
            ConversationState::Working,
            "{label} must stay Working (no Deadlock ECP storm)"
        );
    }
}

#[tokio::test]
async fn large_group_elects_proper_subset() {
    // n = 3, sn_max = 2 → the list is a genuine subset; a voted election
    // picks exactly two stewards, agreed by every node.
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB, CHARLIE],
        "lg",
        fast_test_config(),
        StewardListConfig::new(1, 2).unwrap(),
    )
    .await;

    let sessions: Vec<SessionArc> = (0..3)
        .map(|i| users[i].0.lookup_entry("lg").unwrap().unwrap())
        .collect();

    let reference = steward_set(&sessions[0].read().unwrap().get_member_roles().unwrap());
    assert_eq!(
        reference.len(),
        2,
        "sn_max=2 with 3 members must elect a 2-steward subset, got {reference:?}"
    );
    for (i, s) in sessions.iter().enumerate() {
        let roles = s.read().unwrap().get_member_roles().unwrap();
        assert_eq!(
            steward_set(&roles),
            reference,
            "node {i} must agree on the elected steward subset"
        );
    }
}
