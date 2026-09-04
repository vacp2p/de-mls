use super::sync::{first_zero_timing_field, synced_default_peer_score, validate_conversation_sync};
use crate::{
    DEFAULT_PEER_SCORE,
    engine::handle::Engine,
    protos::de_mls::messages::v1::{
        ConversationSync, ConversationUpdateRequest, StewardElectionProposal, TimingConfig,
    },
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

fn valid_sync_with(threshold: i64) -> ConversationSync {
    ConversationSync {
        steward_members: vec![b"alice".to_vec()],
        election_epoch: 0,
        sn_min: 1,
        sn_max: 5,
        allow_subset_candidates: false,
        peer_scores: vec![],
        timing: Some(nonzero_timing()),
        retry_round: 0,
        liveness_criteria_yes: true,
        threshold_peer_score: threshold,
        pending_update_max_epochs: 3,
        unsettled_members: vec![],
        default_peer_score: 100,
    }
}

#[test]
fn nonzero_timing_passes() {
    assert!(first_zero_timing_field(&nonzero_timing()).is_none());
}

/// The synced pair goes through the same `ScoringConfig::validate` a local
/// config passes at construction: a positive starting score, above the
/// removal threshold.
#[test]
fn validate_accepts_a_sound_peer_score_pair() {
    let sync = valid_sync_with(0);
    assert!(validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap());
}

/// A starting score at or below the threshold would admit members already
/// countable as malicious.
#[test]
fn validate_rejects_a_default_not_above_the_threshold() {
    let mut sync = valid_sync_with(0);
    for threshold in [100, 500] {
        sync.threshold_peer_score = threshold;
        assert!(
            !validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap(),
            "default 100 against threshold {threshold} must be rejected"
        );
    }
}

/// An unset `default_peer_score` — proto3 gives zero, with no way to tell
/// it from "never written" — means the program default, so a sender that
/// predates the field stays joinable.
#[test]
fn validate_reads_an_absent_default_as_the_program_default() {
    let mut sync = valid_sync_with(0);
    sync.default_peer_score = 0;
    assert!(validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()]).unwrap());
    assert_eq!(synced_default_peer_score(&sync), DEFAULT_PEER_SCORE);
}

/// A negative default was written on purpose and is not a score to start
/// from.
#[test]
fn validate_rejects_a_negative_default() {
    let mut sync = valid_sync_with(0);
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
    let mut sync = valid_sync_with(0);
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
    let mut sync = valid_sync_with(0);
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
    ];
    for (name, timing) in cases {
        assert_eq!(
            first_zero_timing_field(&timing),
            Some(name),
            "expected field {name} to be detected as zero"
        );
    }
}

use crate::{
    Event,
    engine::{
        config::EngineConfig,
        store::InMemoryStore,
        test_support::{creator, founder, id, joiner, member},
        types::{Outbound, Timestamp},
    },
    protos::de_mls::messages::v1::MemberInvite,
};

/// The creator is its own steward list, so it answers a sync request by
/// broadcasting the sync a bootstrap-less joiner can adopt.
#[test]
fn epoch_steward_answers_sync_request() {
    let mut engine = creator();
    engine.begin(Timestamp::ZERO);
    engine
        .on_conversation_sync_request(b"joiner")
        .expect("answer request");
    assert!(matches!(
        engine.out.outbound.as_slice(),
        [Outbound::Control(_)]
    ));
}

/// A member with no list can't answer — one answerer keeps one sync on the
/// wire per request.
#[test]
fn non_steward_ignores_sync_request_and_arms_nothing() {
    let mut engine = joiner();
    engine.begin(Timestamp::ZERO);
    engine
        .on_conversation_sync_request(b"joiner")
        .expect("ignore request");
    assert!(engine.out.outbound.is_empty());
    assert!(engine.timing.sync_resend_anchor.is_none());
}

/// The epoch steward answers reactively, so it never leaves a backup
/// takeover armed — a stale anchor is cleared.
#[test]
fn epoch_steward_request_clears_takeover_anchor() {
    let mut engine = creator();
    engine.begin(Timestamp::ZERO);
    engine.timing.sync_resend_anchor = Some(Timestamp::ZERO);
    engine
        .on_conversation_sync_request(b"joiner")
        .expect("answer request");
    assert!(engine.timing.sync_resend_anchor.is_none());
    assert_eq!(engine.out.outbound.len(), 1);
}

/// Sharing is a no-op with no steward list to send yet.
#[test]
fn share_conversation_sync_is_noop_without_list() {
    let mut engine = joiner();
    engine.begin(Timestamp::ZERO);
    engine.share_conversation_sync().expect("share sync");
    assert!(engine.out.outbound.is_empty());
}

/// The pull side puts one request on the wire per window and counts the
/// unanswered rounds.
#[test]
fn sync_request_drive_asks_once_per_window() {
    let mut engine = joiner();
    engine.begin(Timestamp::ZERO);
    engine.drive_sync_request().expect("first request");
    assert_eq!(engine.out.outbound.len(), 1);
    assert_eq!(engine.timing.unanswered_sync_rounds, 1);

    // Inside the window: no second request.
    engine.drive_sync_request().expect("second request");
    assert_eq!(engine.out.outbound.len(), 1);
    assert_eq!(engine.timing.unanswered_sync_rounds, 1);

    let window = engine.config.backup_takeover_window;
    engine.begin(Timestamp::ZERO + window);
    engine.drive_sync_request().expect("third request");
    assert_eq!(engine.out.outbound.len(), 2);
    assert_eq!(engine.timing.unanswered_sync_rounds, 2);
}

/// The sync round-trips: what a steward builds, a listless member adopts
/// off the wire like any other control message.
#[test]
fn conversation_sync_round_trips_into_a_joiner() {
    let mut steward = founder();
    steward.begin(Timestamp::ZERO);
    let bytes = steward
        .build_bootstrap()
        .expect("build sync")
        .expect("a list is installed");

    let mut engine = joiner();
    let out = engine
        .handle_control(Timestamp::ZERO, id("alice"), 1, &bytes)
        .expect("handle control");
    assert!(engine.is_synced());
    assert!(out.events.contains(&Event::SyncApplied));
}

/// The honest list recomputes from the local members; one drawn off them
/// does not.
#[test]
fn validate_election_list_recomputes_from_local_members() {
    let engine = creator();
    let honest = StewardElectionProposal {
        proposed_stewards: vec![engine.own.clone()],
        election_epoch: engine.epoch,
        retry_round: 0,
    };
    assert!(
        engine.validate_election_list(&honest).unwrap(),
        "the honest list matches the local members"
    );
    let forged = StewardElectionProposal {
        proposed_stewards: vec![member("nobody")],
        election_epoch: engine.epoch,
        retry_round: 0,
    };
    assert!(
        !engine.validate_election_list(&forged).unwrap(),
        "a list off the local members is rejected"
    );
}

/// A buffered invite for a seated member and a buffered removal for a
/// stranger are both inert; a removal for a live member is actionable.
#[test]
fn actionable_updates_skip_changes_that_already_landed() {
    let (mut engine, _) = Engine::join(
        "conv",
        id("alice"),
        1,
        &[id("alice"), id("bob")],
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("join");
    engine.begin(Timestamp::ZERO);

    engine.queues.insert_pending_update(
        ConversationUpdateRequest::member_invite(MemberInvite {
            key_package_bytes: b"kp".to_vec(),
            member_id: member("bob"),
        }),
        engine.epoch,
    );
    engine.queues.insert_pending_update(
        ConversationUpdateRequest::remove_member(member("ghost")),
        engine.epoch,
    );
    assert!(
        engine.actionable_buffered_updates().is_empty(),
        "an invite for a seated member and a removal for a stranger do nothing"
    );

    engine.queues.insert_pending_update(
        ConversationUpdateRequest::remove_member(member("alice")),
        engine.epoch,
    );
    assert_eq!(engine.actionable_buffered_updates().len(), 1);
}
