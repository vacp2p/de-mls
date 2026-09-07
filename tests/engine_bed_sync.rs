//! Sync recovery on the fake bed: a backup steward answers a resync when the
//! epoch steward is silent.

mod common;

use common::fake_router::Bed;
use common::fast_config;
use de_mls::engine::{Event, InMemoryStore, Phase};

// A member restarting from an empty store asks for a sync on its own; with
// the epoch steward silent, a backup steward answers it.
#[test]
fn backup_steward_answers_when_the_epoch_steward_is_silent() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    bed.seat(0, "bob");
    bed.seat(0, "carol");
    let dave = bed.seat(0, "dave");

    bed.mute(bed.epoch_steward());

    // An empty store: the engine restarts as `join`, unsynced.
    bed.restart_with(dave, InMemoryStore::default());
    assert!(!bed.router(dave).engine.is_synced());

    bed.process_until("dave resynced by a backup steward", |b| {
        b.router(dave).engine.is_synced()
    });

    assert!(
        bed.router(dave)
            .events
            .iter()
            .any(|e| matches!(e, Event::SyncApplied)),
        "dave adopted a sync from the backup steward"
    );
}

// A `Working` member asks anyway (the pull is not limited to any phase)
// and learns from the answer that its list is current: the only things a
// router can see are its own events and phase, and it sees neither an
// adoption nor a miss.
#[test]
fn a_working_member_can_ask_and_learns_it_is_current() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");

    let n = [0, bob, carol]
        .into_iter()
        .find(|&i| !bed.router(i).engine.is_epoch_steward())
        .expect("a member that is not the epoch steward");
    assert_eq!(bed.router(n).engine.phase(), Phase::Working);

    let events_before = bed.router(n).events.len();
    let now = bed.now;
    bed.router_mut(n)
        .act(now, |e| e.request_sync(now))
        .expect("request sync");

    let window = bed.router(n).engine.config().backup_takeover_window;
    let deadline = bed.now + window * 2;
    bed.process_until("the request settles with no miss", |b| b.now >= deadline);

    assert!(
        !bed.router(n).events[events_before..]
            .iter()
            .any(|e| matches!(e, Event::SyncApplied | Event::SyncUnanswered)),
        "a current node adopts nothing and reports no miss"
    );
    assert_eq!(bed.router(n).engine.phase(), Phase::Working);
}
