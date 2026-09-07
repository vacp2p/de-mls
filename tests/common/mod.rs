//! The fake bed for de-mls integration tests: a cryptography-free MLS group
//! stand-in, the reference router built over it, and virtual-time network
//! delivery. Some methods here are scaffolding for scenarios not yet written.
#![allow(dead_code)]

use std::time::Duration;

use de_mls::EngineConfig;

pub mod fake_mls;
pub mod fake_router;
pub mod net;

/// Timers short enough for `Bed::process_until`'s 30-virtual-second budget:
/// the shared config for scenarios that need a round to actually run rather
/// than sit in `Phase::Working`.
pub fn fast_config() -> EngineConfig {
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
