//! Caller-owned time source (RFC issue #129).
//!
//! Every deadline in the library — commit/recovery inactivity, the freeze
//! window, consensus timeouts, auto-votes — is measured against a
//! [`WallClock`] the integrator supplies at construction, so the
//! application controls where time comes from: [`SystemClock`] in
//! production, [`MockClock`] in tests. The library never reads the system
//! clock outside [`SystemClock`].

use std::{
    ops::Add,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A point in time: milliseconds since the Unix epoch.
///
/// Unlike [`std::time::Instant`], a `Timestamp` can be fabricated, which is
/// what lets tests drive virtual time. [`Timestamp::as_secs`] yields the
/// wire-format value carried in consensus proposals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(Duration);

impl Timestamp {
    pub const ZERO: Timestamp = Timestamp(Duration::ZERO);

    /// A timestamp `d` past the Unix epoch.
    pub fn from_duration_since_epoch(d: Duration) -> Self {
        Timestamp(d)
    }

    /// Whole seconds since the Unix epoch — the consensus wire format.
    pub fn as_secs(&self) -> u64 {
        self.0.as_secs()
    }

    /// Milliseconds since the Unix epoch.
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

/// The conversation's time source, owned by the integrator and moved in at
/// construction like the other services. All deadline math and the
/// consensus wire timestamps derive from [`now`](WallClock::now).
pub trait WallClock {
    /// The current time. Successive calls must be non-decreasing.
    fn now(&self) -> Timestamp;
}

/// Production clock backed by [`SystemTime`], clamped to be monotone
/// non-decreasing so an OS clock step backwards can't un-elapse a deadline.
/// Clones share the clamp state.
#[derive(Debug, Clone, Default)]
pub struct SystemClock {
    /// Highest millisecond value handed out so far.
    floor_ms: Arc<AtomicU64>,
}

impl SystemClock {
    pub fn new() -> Self {
        Self::default()
    }
}

impl WallClock for SystemClock {
    fn now(&self) -> Timestamp {
        let system_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as u64;
        let prev = self.floor_ms.fetch_max(system_ms, Ordering::Relaxed);
        Timestamp(Duration::from_millis(system_ms.max(prev)))
    }
}

/// Manually driven clock for tests. Clones share the same underlying time,
/// so a test keeps one handle to [`advance`](MockClock::advance) while the
/// conversation owns another.
///
/// Starts at [`Timestamp::ZERO`]; use [`MockClock::at`] to start elsewhere
/// (e.g. real time, or skewed relative to another member's clock).
#[derive(Debug, Clone, Default)]
pub struct MockClock {
    now_ms: Arc<AtomicU64>,
}

impl MockClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// A clock starting at `start`.
    pub fn at(start: Timestamp) -> Self {
        Self {
            now_ms: Arc::new(AtomicU64::new(start.as_millis())),
        }
    }

    /// Move time forward by `d`. Visible to every clone immediately.
    pub fn advance(&self, d: Duration) {
        self.now_ms
            .fetch_add(d.as_millis() as u64, Ordering::Relaxed);
    }

    /// Jump to an absolute time. Backward jumps are allowed — skewing a
    /// member's clock is the point of this type — but deadline math on a
    /// single conversation assumes its own clock never runs backwards.
    pub fn set(&self, t: Timestamp) {
        self.now_ms.store(t.as_millis(), Ordering::Relaxed);
    }
}

impl WallClock for MockClock {
    fn now(&self) -> Timestamp {
        Timestamp(Duration::from_millis(self.now_ms.load(Ordering::Relaxed)))
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
        assert_eq!(clock.now(), Timestamp::ZERO + Duration::from_secs(5));
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
    fn system_clock_is_monotone_across_clones() {
        let clock = SystemClock::new();
        let a = clock.now();
        let b = clock.clone().now();
        assert!(b >= a);
    }

    #[test]
    fn wire_seconds_truncate_milliseconds() {
        let t = Timestamp::from_duration_since_epoch(Duration::from_millis(1999));
        assert_eq!(t.as_secs(), 1);
    }
}
