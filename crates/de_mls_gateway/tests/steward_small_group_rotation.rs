//! Steward-list reconcile under the settled-member rule: a just-bootstrapped
//! group (every joiner added this epoch) keeps the creator as sole steward and
//! fires no election. These tests pin the behavior — stays `Working`, a
//! committer always exists, `retry_round` never leaves 0 (no election storm).

use std::time::Duration;

use de_mls::core::{ConversationState, StewardListConfig};

mod common;
use common::session_fixtures::{
    SessionArc, TestUser, TransportHandle, bootstrap_joined_conversation, deliver,
    fast_test_config, flush_user, poll_once, settle_for,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";

/// Drain every transport and feed each packet to every user, mirroring the
/// gateway relay. Echo-dedup drops a user's own packets.
fn relay_all(users: &[(TestUser, TransportHandle)]) {
    for (u, h) in users {
        flush_user(u, h);
    }
    let mut packets = Vec::new();
    for (_, h) in users {
        packets.extend(h.lock().unwrap().drain_packets());
    }
    for p in &packets {
        for (u, _) in users {
            deliver(u, p);
        }
    }
}

/// Poll past every deadline, then assert stability: a committer exists, no
/// election fired (`retry_round == 0`), every member stays `Working`.
fn assert_stable_no_election(
    users: &[(TestUser, TransportHandle)],
    conversation: &str,
    sessions: &[(&str, &SessionArc)],
) {
    assert!(
        sessions[0].1.read().unwrap().is_steward(),
        "creator must be a steward (a committer must always exist)"
    );

    for _ in 0..20 {
        settle_for(Duration::from_millis(40));
        for (_, s) in sessions {
            poll_once(s);
        }
        relay_all(users);
    }

    for (label, s) in sessions {
        let (_, retry) = s.read().unwrap().epoch_and_retry().unwrap();
        assert_eq!(
            retry, 0,
            "{label} retry_round must stay 0 — no election fires for unsettled members ({conversation})"
        );
        assert_eq!(
            s.read().unwrap().conversation_state(),
            ConversationState::Working,
            "{label} must stay Working ({conversation})"
        );
    }
}

#[test]
fn small_group_reconciles_locally_no_election() {
    // n = 2, sn_max = 3 → list fits; reconciled locally, no vote.
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB],
        "sg",
        fast_test_config(),
        StewardListConfig::new(1, 3).unwrap(),
    );
    let alice = users[0].0.lookup_entry("sg").unwrap().unwrap();
    let bob = users[1].0.lookup_entry("sg").unwrap().unwrap();
    assert_stable_no_election(&users, "sg", &[("alice", &alice), ("bob", &bob)]);
}

#[test]
fn members_over_sn_max_do_not_elect_until_settled() {
    // n = 3, sn_max = 2 → more members than sn_max, but the joiners aren't
    // settled yet, so settled members still fit: no premature election.
    let users = bootstrap_joined_conversation(
        &[ALICE, BOB, CHARLIE],
        "lg",
        fast_test_config(),
        StewardListConfig::new(1, 2).unwrap(),
    );
    let s: Vec<SessionArc> = (0..3)
        .map(|i| users[i].0.lookup_entry("lg").unwrap().unwrap())
        .collect();
    assert_stable_no_election(
        &users,
        "lg",
        &[("alice", &s[0]), ("bob", &s[1]), ("charlie", &s[2])],
    );
}
