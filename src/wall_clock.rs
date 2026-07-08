//! Caller-supplied time source.
//!
//! Every deadline in the library — commit/recovery inactivity, the freeze
//! window, consensus timeouts, auto-votes — is measured against a
//! [`WallClock`] the integrator supplies at construction, so the
//! application controls where time comes from: its own production impl
//! (typically wrapping [`std::time::SystemTime`]), or [`MockClock`] in
//! tests.

use std::{
    ops::Add,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

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

/// The conversation's time source. The integrator picks the impl and moves
/// an instance in at construction like the other services; the conversation
/// owns it from then on. All deadline math and the consensus wire
/// timestamps derive from [`now`](WallClock::now).
pub trait WallClock {
    /// The current time as a duration since the Unix epoch. Successive
    /// readings must never decrease — a reading that runs backwards
    /// un-elapses pending deadlines and can break the timestamp ordering
    /// peers validate votes against.
    ///
    /// This is weaker than a monotonic clock: forward jumps, freezes, and
    /// slew are all fine; only backwards readings are ruled out. A
    /// [`std::time::SystemTime`]-backed impl should clamp against backwards
    /// steps (return the max of the last reading and now).
    fn now(&self) -> Duration;
}

/// Internal bridge from the integrator-facing [`WallClock`] (plain
/// [`Duration`] since the Unix epoch) to the [`Timestamp`] the library's
/// deadline math runs on. Blanket-implemented for every clock, so external
/// impls never construct the wrapper type.
pub(crate) trait WallClockExt: WallClock {
    /// [`WallClock::now`] as a [`Timestamp`].
    fn timestamp(&self) -> Timestamp {
        Timestamp::from_duration_since_epoch(self.now())
    }
}

impl<Wc: WallClock> WallClockExt for Wc {}

/// Manually driven clock for tests, nanosecond-precise. Clones share the
/// same underlying  time, so a test keeps one handle to [`advance`](MockClock::advance)
/// while the conversation owns another.
///
/// Starts at the Unix epoch; use [`MockClock::at`] to start elsewhere
/// (e.g. real time, or skewed relative to another member's clock).
#[derive(Debug, Clone, Default)]
pub struct MockClock {
    now_nanos: Arc<AtomicU64>,
}

/// Truncate a [`Duration`] into the `MockClock` nanosecond range.
fn duration_nanos(d: Duration) -> u64 {
    d.as_nanos().min(u64::MAX as u128) as u64
}

impl MockClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// A clock starting at `start` past the Unix epoch.
    pub fn at(start: Duration) -> Self {
        Self {
            now_nanos: Arc::new(AtomicU64::new(duration_nanos(start))),
        }
    }

    /// Move time forward by `d` (saturating). Visible to every clone
    /// immediately.
    pub fn advance(&self, d: Duration) {
        let add = duration_nanos(d);
        let _ = self
            .now_nanos
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_add(add))
            });
    }
}

impl WallClock for MockClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.now_nanos.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_clock_clones_share_time() {
        let clock = MockClock::new();
        let handle = clock.clone();
        handle.advance(Duration::from_secs(5));
        assert_eq!(clock.now(), Duration::from_secs(5));
    }

    #[test]
    fn mock_clock_advance_is_sub_millisecond_exact() {
        let clock = MockClock::new();
        clock.advance(Duration::from_micros(500));
        clock.advance(Duration::from_micros(500));
        assert_eq!(clock.now(), Duration::from_millis(1));
    }

    #[test]
    fn mock_clock_at_reproduces_the_start_exactly() {
        let start = Duration::new(1_750_000_000, 123_456);
        assert_eq!(MockClock::at(start).now(), start);
    }

    #[test]
    fn blanket_timestamp_bridges_now_into_deadline_math() {
        let clock = MockClock::at(Duration::from_secs(7));
        assert_eq!(
            clock.timestamp(),
            Timestamp::from_duration_since_epoch(Duration::from_secs(7))
        );
    }

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
