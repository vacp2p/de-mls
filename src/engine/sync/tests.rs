use std::time::Duration;

use prost::Message;

use super::adopt::{
    first_zero_timing_field, synced_default_peer_score, validate_conversation_sync,
};
use crate::{
    DEFAULT_PEER_SCORE, Decision, Engine, EngineConfig, Event, InMemoryStore, StewardList,
    StewardListConfig,
    engine::{
        test_support::{creator, founder, id, joiner, member},
        types::{CommitHash, Outbound, Phase, StagedFacts, Timestamp},
    },
    protos::de_mls::messages::v1::{
        ControlMessage, ConversationSync, JoinEpoch, TimingConfig, control_message,
    },
};

fn at(secs: u64) -> Timestamp {
    Timestamp::from_duration_since_epoch(Duration::from_secs(secs))
}

/// Decode the `ConversationSync` payload out of the control bytes a steward
/// built.
fn decode_sync(bytes: &[u8]) -> ConversationSync {
    match ControlMessage::decode(bytes).expect("decode").payload {
        Some(control_message::Payload::ConversationSync(sync)) => sync,
        other => panic!("expected a ConversationSync payload, got {other:?}"),
    }
}

fn join_epoch(member_id: &[u8], epoch: u64) -> JoinEpoch {
    JoinEpoch {
        member_id: member_id.to_vec(),
        epoch,
    }
}

fn list_config(sn_min: usize, sn_max: usize) -> StewardListConfig {
    StewardListConfig::new(sn_min, sn_max).expect("list config")
}

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
        join_epochs: vec![],
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

/// The carried join epochs reconstruct the pool, so a list naming a member
/// seated at the election epoch is rejected even though it is internally
/// well-ordered.
#[test]
fn validate_verifies_list_against_reconstructed_pool() {
    let members = vec![b"alice".to_vec(), b"bob".to_vec()];
    let mut sync = valid_sync();
    sync.sn_max = 1;
    // bob was seated at the election epoch → unsettled → pool = [alice].
    sync.join_epochs = vec![join_epoch(b"bob", 0)];

    // The list correctly derived from the pool is accepted.
    sync.steward_members = vec![b"alice".to_vec()];
    assert!(validate_conversation_sync("g", &sync, 0, &members).unwrap());

    // A list naming the excluded member does not re-derive → rejected.
    sync.steward_members = vec![b"bob".to_vec()];
    assert!(!validate_conversation_sync("g", &sync, 0, &members).unwrap());
}

/// A fabricated join epoch for an id not in the current set is rejected,
/// so a peer can't shape the reconstructed pool with ids that aren't
/// members.
#[test]
fn validate_rejects_join_epoch_not_in_members() {
    let members = vec![b"alice".to_vec(), b"bob".to_vec()];
    let mut sync = valid_sync();
    sync.sn_max = 1;
    sync.steward_members = vec![b"alice".to_vec()];
    sync.join_epochs = vec![join_epoch(b"ghost", 0)];
    assert!(!validate_conversation_sync("g", &sync, 0, &members).unwrap());
}

/// A member seated before the election epoch is in the pool: a two-member
/// list over both is accepted, and it cannot be re-derived from alice
/// alone, so leaving bob out would reject it.
#[test]
fn validate_settles_by_join_epoch_against_election_epoch() {
    let members = vec![b"alice".to_vec(), b"bob".to_vec()];
    let mut sync = valid_sync();
    sync.sn_max = 2;
    sync.election_epoch = 2;
    sync.join_epochs = vec![join_epoch(b"bob", 1)];
    sync.steward_members = StewardList::generate(2, b"g", &members, 2, list_config(1, 2), 0)
        .expect("list")
        .members()
        .to_vec();
    assert!(validate_conversation_sync("g", &sync, 2, &members).unwrap());
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

/// A node that is already `Working` — its own list still covers the epoch —
/// adopts an answer whose election is strictly newer, in any phase: the
/// pull is not limited to `Syncing`.
#[test]
fn a_working_node_adopts_a_newer_election() {
    let members = [id("alice"), id("bob"), id("carol")];
    // B's list, elected at 0, has two stewards (default sn_max) and so
    // covers epochs 0-1: setting its epoch to 1 leaves it `Working`.
    let mut b = Engine::create(
        at(0),
        "conv",
        id("alice"),
        0,
        &members,
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create")
    .0;
    b.epoch = 1;
    assert_eq!(b.phase(), Phase::Working);

    let a = Engine::create(
        at(0),
        "conv",
        id("alice"),
        1,
        &members,
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create")
    .0;
    let bytes = a.build_conversation_sync().expect("a list is installed");
    let sync = decode_sync(&bytes);

    b.begin(at(5));
    b.broadcast_sync_request();
    b.on_conversation_sync(sync).expect("adopt");

    assert_eq!(b.steward_list.election_epoch(), Some(1));
    assert!(b.out.events.contains(&Event::SyncApplied));
    assert_eq!(b.phase(), Phase::Working);
    assert!(b.timing.sync_deadline.is_none());
}

/// A current node — its own list is from this same election or a newer one
/// — still counts the answer against the request it sent: nothing is
/// adopted, but the pending deadline is settled and no miss follows.
#[test]
fn a_current_node_counts_an_answer_and_reports_no_miss() {
    let members = [id("alice"), id("bob"), id("carol")];
    let mut a = Engine::create(
        at(0),
        "conv",
        id("alice"),
        1,
        &members,
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create")
    .0;

    a.begin(at(0));
    a.broadcast_sync_request();
    let bytes = a.build_conversation_sync().expect("a list is installed");
    let sync = decode_sync(&bytes);

    a.on_conversation_sync(sync)
        .expect("answered, nothing newer");
    assert!(!a.out.events.contains(&Event::SyncApplied));
    assert!(a.timing.sync_deadline.is_none());

    a.begin(at(0) + a.config.backup_takeover_window * 2);
    a.drive_sync_request();
    assert!(!a.out.events.contains(&Event::SyncUnanswered));
}

/// A candidate held while `Freezing` is re-checked against the newly
/// adopted list: one from anyone but its epoch steward is discarded and the
/// round reopens on `Working`. The lists here may name the same steward;
/// the sender is chosen to be someone else.
#[test]
fn adopting_a_newer_list_drops_a_candidate_from_the_old_steward() {
    let members = [id("alice"), id("bob"), id("carol")];
    let mut b = Engine::create(
        at(0),
        "conv",
        id("alice"),
        0,
        &members,
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create")
    .0;
    b.epoch = 1;

    let a = Engine::create(
        at(0),
        "conv",
        id("alice"),
        1,
        &members,
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create")
    .0;
    let bytes = a.build_conversation_sync().expect("a list is installed");
    let sync = decode_sync(&bytes);

    let new_steward = a.expected_steward().expect("a steward at epoch 1");
    let stale_sender = ["alice", "bob", "carol"]
        .into_iter()
        .find(|m| member(m) != new_steward)
        .expect("at least one member isn't the new epoch steward");

    b.begin(at(5));
    b.broadcast_sync_request();
    b.start_freezing();
    let hash = CommitHash::of(b"stale candidate");
    b.round_candidate = Some((
        hash,
        StagedFacts {
            sender: id(stale_sender),
            epoch: 1,
            actions: vec![],
            proposal_count: 0,
            self_removed: false,
        },
    ));

    b.on_conversation_sync(sync).expect("adopt");

    assert!(
        b.out
            .decisions
            .iter()
            .any(|d| matches!(d, Decision::Discard { hashes } if hashes == &vec![hash]))
    );
    assert!(b.round_candidate.is_none());
    assert_eq!(b.phase(), Phase::Working);
    assert!(b.out.events.contains(&Event::PhaseChange(Phase::Working)));
}
