//! Sync recovery on the fake bed: a backup steward answers a resync when the
//! epoch steward is silent.

mod common;

use common::fake_router::Bed;
use common::fast_config;
use de_mls::engine::{Event, InMemoryStore};

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
