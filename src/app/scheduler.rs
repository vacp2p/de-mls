//! Configurable steward epoch scheduling.
//!
//! Provides a [`StewardScheduler`] trait and a default [`IntervalScheduler`]
//! implementation that ticks at a fixed interval.

use async_trait::async_trait;
use std::time::Duration;

/// Default epoch duration (30 seconds).
pub const DEFAULT_EPOCH_DURATION: Duration = Duration::from_secs(30);

/// Configuration for steward epoch scheduling.
pub struct StewardSchedulerConfig {
    /// Interval between steward epochs.
    pub epoch_interval: Duration,
}

impl Default for StewardSchedulerConfig {
    fn default() -> Self {
        Self {
            epoch_interval: DEFAULT_EPOCH_DURATION,
        }
    }
}

/// Trait for controlling when steward epochs fire.
///
/// Implementations can use timers, thresholds, external triggers, etc.
#[async_trait]
pub trait StewardScheduler: Send + Sync {
    /// Wait until the next epoch should begin.
    async fn next_tick(&mut self);
}

/// A simple interval-based scheduler.
pub struct IntervalScheduler {
    interval: tokio::time::Interval,
}

impl IntervalScheduler {
    pub fn new(config: StewardSchedulerConfig) -> Self {
        Self {
            interval: tokio::time::interval(config.epoch_interval),
        }
    }
}

#[async_trait]
impl StewardScheduler for IntervalScheduler {
    async fn next_tick(&mut self) {
        self.interval.tick().await;
    }
}
