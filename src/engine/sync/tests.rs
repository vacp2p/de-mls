use std::time::Duration;

use super::adopt::{
    first_zero_timing_field, synced_default_peer_score, validate_conversation_sync,
};
use crate::{
    DEFAULT_PEER_SCORE, Engine, EngineConfig, Event, InMemoryStore,
    engine::{
        test_support::{creator, founder, id, joiner},
        types::{Outbound, Phase, Timestamp},
    },
    protos::de_mls::messages::v1::{ConversationSync, TimingConfig},
};

fn nonzero_timing() -> TimingConfig {
    TimingConfig {
        commit_batch_window_ms: 60_000,
        freeze_duration_ms: 30_000,
        proposal_expiration_ms: 3_600_000,
        consensus_timeout_ms: 30_000,
        backup_takeover_window_ms: 30_000,
    }
}

fn valid_sync() -> ConversationSync {
    ConversationSync {
        steward_members: vec![b"alice".to_vec()],
        election_epoch: 0,
        sn_min: 1,
        sn_max: 5,
        peer_scores: vec![],
        timing: Some(nonzero_timing()),
        retry_round: 0,
        liveness_criteria_yes: true,
        unsettled_members: vec![],
        default_peer_score: 100,
    }
}

#[test]
fn nonzero_timing_passes() {
    assert!(first_zero_timing_field(&nonzero_timing()).is_none());
}

/// The synced default goes through the same `ScoringConfig::validate` a
/// local config passes at construction: a positive starting score.
#[test]
fn validate_accepts_a_positive_default_score() {
    let sync = valid_sync();
    assert!(validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap());
}

/// An unset `default_peer_score` — proto3 gives zero, with no way to tell
/// it from "never written" — means the program default, so a sender that
/// predates the field stays joinable.
#[test]
fn validate_reads_an_absent_default_as_the_program_default() {
    let mut sync = valid_sync();
    sync.default_peer_score = 0;
    assert!(validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap());
    assert_eq!(synced_default_peer_score(&sync), DEFAULT_PEER_SCORE);
}

/// A negative default was written on purpose and is not a score to start
/// from.
#[test]
fn validate_rejects_a_negative_default() {
    let mut sync = valid_sync();
    for default in [-1, i64::MIN] {
        sync.default_peer_score = default;
        assert!(
            !validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap(),
            "default_peer_score {default} must be rejected"
        );
    }
}

/// The carried delta reconstructs the pool, so a list naming a member the
/// delta excludes is rejected even though it is internally well-ordered.
#[test]
fn validate_verifies_list_against_reconstructed_pool() {
    let members = vec![b"alice".to_vec(), b"bob".to_vec()];
    let mut sync = valid_sync();
    sync.sn_max = 1;
    // bob is unsettled → excluded → pool = [alice].
    sync.unsettled_members = vec![b"bob".to_vec()];

    // The list correctly derived from the pool is accepted.
    sync.steward_members = vec![b"alice".to_vec()];
    assert!(validate_conversation_sync("g", &sync, 0, &members).unwrap());

    // A list naming the excluded member does not re-derive → rejected.
    sync.steward_members = vec![b"bob".to_vec()];
    assert!(!validate_conversation_sync("g", &sync, 0, &members).unwrap());
}

/// A fabricated `unsettled_members` id (not in the current set) is
/// rejected, so a peer can't shrink the reconstructed pool with ids that
/// aren't members.
#[test]
fn validate_rejects_unsettled_not_in_members() {
    let members = vec![b"alice".to_vec(), b"bob".to_vec()];
    let mut sync = valid_sync();
    sync.sn_max = 1;
    sync.steward_members = vec![b"alice".to_vec()];
    sync.unsettled_members = vec![b"ghost".to_vec()];
    assert!(!validate_conversation_sync("g", &sync, 0, &members).unwrap());
}

#[test]
fn each_zero_field_is_detected() {
    let cases = [
        (
            "commit_batch_window_ms",
            TimingConfig {
                commit_batch_window_ms: 0,
                ..nonzero_timing()
            },
        ),
        (
            "freeze_duration_ms",
            TimingConfig {
                freeze_duration_ms: 0,
                ..nonzero_timing()
            },
        ),
        (
            "proposal_expiration_ms",
            TimingConfig {
                proposal_expiration_ms: 0,
                ..nonzero_timing()
            },
        ),
        (
            "consensus_timeout_ms",
            TimingConfig {
                consensus_timeout_ms: 0,
                ..nonzero_timing()
            },
        ),
        (
            "backup_takeover_window_ms",
            TimingConfig {
                backup_takeover_window_ms: 0,
                ..nonzero_timing()
            },
        ),
    ];
    for (name, timing) in cases {
        assert_eq!(
            first_zero_timing_field(&timing),
            Some(name),
            "expected field {name} to be detected as zero"
        );
    }
}

/// The creator is its own steward list, so it answers a sync request by
/// sharing the sync a joiner with no list can adopt.
#[test]
fn epoch_steward_answers_sync_request() {
    let mut engine = creator();
    engine.begin(Timestamp::ZERO);
    engine.on_conversation_sync_request(b"joiner");
    assert!(matches!(
        engine.out.outbound.as_slice(),
        [Outbound::Control(_)]
    ));
}

/// A member with no list can't answer.
#[test]
fn an_unsynced_node_ignores_sync_request_and_arms_nothing() {
    let mut engine = joiner();
    engine.begin(Timestamp::ZERO);
    engine.on_conversation_sync_request(b"joiner");
    assert!(engine.out.outbound.is_empty());
    assert!(engine.timing.sync_takeover_anchor.is_none());
}

/// A synced member off the list can't answer either — one answerer keeps
/// one sync on the wire per request.
#[test]
fn a_synced_non_steward_ignores_sync_request_and_arms_nothing() {
    let mut engine = Engine::create(
        Timestamp::ZERO,
        "conv",
        id("bob"),
        0,
        &[id("alice"), id("bob")],
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create")
    .0;
    engine.begin(Timestamp::ZERO);
    engine
        .steward_list
        .install_list(0, &[b"alice".to_vec()], 1, 0)
        .expect("a list of alice alone");
    assert!(engine.is_synced() && !engine.is_steward());

    engine.on_conversation_sync_request(b"joiner");
    assert!(engine.out.outbound.is_empty());
    assert!(engine.timing.sync_takeover_anchor.is_none());
}

/// An unsynced node cannot judge whether it is the epoch steward or a
/// backup, so it answers nothing and arms nothing.
#[test]
fn an_unsynced_steward_does_not_answer() {
    let mut engine = joiner();
    engine.begin(Timestamp::ZERO);
    engine.on_conversation_sync_request(b"x");
    assert!(engine.out.outbound.is_empty());
    assert!(engine.timing.sync_takeover_anchor.is_none());
}

/// The epoch steward answers reactively, so it never leaves a backup
/// takeover armed — a stale anchor is cleared.
#[test]
fn epoch_steward_request_clears_takeover_anchor() {
    let mut engine = creator();
    engine.begin(Timestamp::ZERO);
    engine.timing.sync_takeover_anchor = Some(Timestamp::ZERO);
    engine.on_conversation_sync_request(b"joiner");
    assert!(engine.timing.sync_takeover_anchor.is_none());
    assert_eq!(engine.out.outbound.len(), 1);
}

/// Sharing is a no-op with no steward list to send yet.
#[test]
fn share_conversation_sync_is_noop_without_list() {
    let mut engine = joiner();
    engine.begin(Timestamp::ZERO);
    engine.share_conversation_sync();
    assert!(engine.out.outbound.is_empty());
}

/// A joiner already asked at `join`: the window it is waiting out reports
/// once when it passes, and the pull side asks again only when the router
/// does.
#[test]
fn a_sync_request_is_reported_missing_once() {
    let mut engine = joiner();
    let turns = engine.config.backup_takeover_window * 2;

    engine.begin(Timestamp::ZERO);
    engine.drive_sync_request();
    assert!(engine.out.events.is_empty());

    engine.begin(Timestamp::ZERO + turns);
    engine.drive_sync_request();
    assert!(engine.out.events.contains(&Event::SyncUnanswered));
    assert!(engine.timing.sync_deadline.is_none());
    assert_eq!(engine.phase(), Phase::Syncing);

    engine.begin(Timestamp::ZERO + turns * 2);
    engine.drive_sync_request();
    assert_eq!(
        engine
            .out
            .events
            .iter()
            .filter(|e| matches!(e, Event::SyncUnanswered))
            .count(),
        1,
        "reported once, not again on its own"
    );

    // Drain what accumulated so far: the router's ask below is measured on
    // its own.
    engine.finish().expect("finish");

    let out = engine
        .request_sync(Timestamp::ZERO + turns * 2)
        .expect("request sync");
    assert_eq!(out.outbound.len(), 1, "every further ask is the router's");

    engine.begin(Timestamp::ZERO + turns * 3);
    engine.drive_sync_request();
    assert!(engine.out.events.contains(&Event::SyncUnanswered));
}

/// The sync round-trips: what a steward builds, a member with no list
/// adopts off the wire like any other control message, leaving `Syncing`
/// for `Working`.
#[test]
fn conversation_sync_round_trips_into_a_joiner() {
    let mut steward = founder();
    steward.begin(Timestamp::ZERO);
    let bytes = steward
        .build_conversation_sync()
        .expect("a list is installed");

    let mut engine = joiner();
    assert_eq!(engine.phase(), Phase::Syncing);
    let out = engine
        .handle_control(Timestamp::ZERO, id("alice"), 1, &bytes)
        .expect("handle control");
    assert!(engine.is_synced());
    assert_eq!(engine.phase(), Phase::Working);
    assert!(out.events.contains(&Event::SyncApplied));
    assert!(out.events.contains(&Event::PhaseChange(Phase::Working)));
    assert!(engine.timing.sync_deadline.is_none());
}

/// Adopting a sync whose `consensus_timeout` sits below this node's own
/// `voting_delay` clamps `voting_delay` to half the new timeout, so the
/// auto-vote still fires before the session closes.
#[test]
fn adopting_a_sync_clamps_voting_delay_below_its_timeout() {
    let founder_config = EngineConfig {
        consensus_timeout: Duration::from_secs(5),
        voting_delay: Duration::from_secs(1),
        ..EngineConfig::default()
    };
    let mut steward = Engine::create(
        Timestamp::ZERO,
        "conv",
        id("alice"),
        1,
        &[id("alice"), id("bob")],
        founder_config,
        InMemoryStore::default(),
    )
    .expect("create")
    .0;
    steward.begin(Timestamp::ZERO);
    let bytes = steward
        .build_conversation_sync()
        .expect("a list is installed");

    let joiner_config = EngineConfig {
        voting_delay: Duration::from_secs(10),
        ..EngineConfig::default()
    };
    let mut engine = Engine::join(
        Timestamp::ZERO,
        "conv",
        id("bob"),
        1,
        &[id("alice"), id("bob")],
        joiner_config,
        InMemoryStore::default(),
    )
    .expect("join")
    .0;
    engine
        .handle_control(Timestamp::ZERO, id("alice"), 1, &bytes)
        .expect("handle control");

    assert_eq!(engine.config.voting_delay, Duration::from_millis(2_500));
}
