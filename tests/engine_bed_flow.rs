//! The engine end to end on the fake bed: a proposal is voted, the epoch
//! steward builds, the round closes, the joiner is welcomed and bootstrapped,
//! and a second add driven by a non-steward converges the whole group.

mod common;

use std::time::Duration;

use common::fake_router::Bed;
use de_mls::EngineConfig;
use de_mls::engine::{Event, Phase};

fn fast() -> EngineConfig {
    EngineConfig {
        commit_batch_window: Duration::from_millis(200),
        freeze_duration: Duration::from_millis(150),
        voting_delay: Duration::from_millis(100),
        consensus_timeout: Duration::from_millis(500),
        proposal_expiration: Duration::from_secs(5),
        backup_takeover_window: Duration::from_millis(300),
        ..EngineConfig::default()
    }
}

#[test]
fn creator_adds_one_then_a_member_adds_another() {
    let mut bed = Bed::new("conv", "alice", fast());
    let bob = bed.add_pending("bob");
    let kp_bob = bed.key_package_of(bob);
    let bob_id = bed.nodes[bob].own.clone();

    let now = bed.now;
    bed.router_mut(0)
        .act(now, |e| e.propose_add(now, bob_id.clone(), kp_bob))
        .expect("propose bob");

    bed.process_until("bob seated", |b| b.is_live(bob) && b.epochs_agree());
    assert_eq!(bed.router(0).mls.epoch(), 1);
    assert!(bed.converged() && bed.membership_agrees());
    assert!(
        bed.router(0)
            .events
            .iter()
            .any(|e| matches!(e, Event::MembersChanged { added, .. } if !added.is_empty())),
        "the creator reports the membership change"
    );
    bed.process_until("bob synced", |b| b.router(bob).engine.is_synced());

    // A non-steward proposes; the steward auto-votes and builds.
    let carol = bed.add_pending("carol");
    let kp_carol = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(bob)
        .act(now, |e| e.propose_add(now, carol_id.clone(), kp_carol))
        .expect("propose carol");

    bed.process_until("carol seated", |b| b.is_live(carol) && b.epochs_agree());
    assert_eq!(bed.router(0).mls.epoch(), 2);
    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(carol).mls.members().len(), 3);
    assert!(
        bed.router(bob)
            .decisions
            .iter()
            .any(|d| matches!(d, de_mls::engine::Decision::Merge { .. })),
        "bob merged the steward's commit"
    );
    assert!(
        !bed.router(bob)
            .decisions
            .iter()
            .any(|d| matches!(d, de_mls::engine::Decision::BuildCommit { .. })),
        "a non-steward never builds"
    );
}

/// The engine keeps no re-propose memory: an announcement made while a
/// commit round is open is refused with `ConversationBlocked`, held by the
/// router, and proposed once the round closes — seating that member in a
/// later epoch than the one the open round resolved.
#[test]
fn announcement_during_a_round_is_retried_after_it_closes() {
    let mut bed = Bed::new("conv", "alice", fast());
    let bob = bed.add_pending("bob");
    let kp_bob = bed.key_package_of(bob);
    let bob_id = bed.nodes[bob].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, bob_id.clone(), kp_bob);
    bed.process_until("bob seated", |b| b.is_live(bob) && b.epochs_agree());
    bed.process_until("bob synced", |b| b.router(bob).engine.is_synced());

    // Open a round: announce carol, then wait until the round is Freezing.
    let carol = bed.add_pending("carol");
    let kp_carol = bed.key_package_of(carol);
    let carol_id = bed.nodes[carol].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, carol_id.clone(), kp_carol);
    bed.process_until("round open", |b| {
        b.router(0).engine.phase() == Phase::Freezing
    });

    // Announce dave while the round is open: the engine refuses
    // `ConversationBlocked`, and the router parks the announcement rather
    // than proposing it.
    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, dave_id.clone(), kp_dave);

    bed.process_until("carol seated", |b| b.is_live(carol) && b.epochs_agree());
    let epoch_after_carol = bed.router(0).mls.epoch();

    // The parked announcement fires once the round closes: dave is seated
    // in a later epoch.
    bed.process_until("dave seated", |b| b.is_live(dave) && b.epochs_agree());
    assert!(bed.router(0).mls.epoch() > epoch_after_carol);
    assert!(bed.converged() && bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 4);
}
