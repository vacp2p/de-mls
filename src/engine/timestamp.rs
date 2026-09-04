//! A point in time, as the router's caller-supplied clock reports it.
//!
//! Every deadline the engine tracks — commit inactivity, the freeze window,
//! consensus timeouts, auto-votes — is measured against the [`Timestamp`]
//! the router passes into each driving call. The engine keeps no clock of
//! its own.

use std::{ops::Add, time::Duration};

/// A point in time: a duration since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(Duration);

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp(Duration::ZERO);

    /// A timestamp `d` past the Unix epoch.
    pub const fn from_duration_since_epoch(d: Duration) -> Self {
        Timestamp(d)
    }

    /// Whole seconds since the Unix epoch — the consensus wire format.
    pub fn as_secs(&self) -> u64 {
        self.0.as_secs()
    }

    /// Milliseconds since the Unix epoch, truncated to `u64`.
    pub fn as_millis(&self) -> u64 {
        self.0.as_millis() as u64
    }

    /// Time elapsed from `earlier` to `self`, or [`Duration::ZERO`] when
    /// `earlier` is in the future.
    pub fn saturating_duration_since(&self, earlier: Timestamp) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

impl Add<Duration> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: Duration) -> Timestamp {
        Timestamp(self.0.saturating_add(rhs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_ordering_and_arithmetic() {
        let t0 = Timestamp::ZERO + Duration::from_millis(100);
        let t1 = t0 + Duration::from_millis(50);
        assert!(t1 > t0);
        assert_eq!(t1.saturating_duration_since(t0), Duration::from_millis(50));
        assert_eq!(t0.saturating_duration_since(t1), Duration::ZERO);
    }

    #[test]
    fn wire_seconds_truncate_milliseconds() {
        let t = Timestamp::from_duration_since_epoch(Duration::from_millis(1999));
        assert_eq!(t.as_secs(), 1);
    }
}
