//! App-side phase timer.
//!
//! Holds the wall-clock anchor (`started_at`). Phase-anchor durations live
//! in [`crate::core::ConversationConfig`] (single source of truth);

use std::time::{Duration, Instant};

/// Wall-clock anchor for the active phase. Holds only the anchor
/// `Instant`; queries take the relevant `Duration` as a parameter.
/// [`crate::session::Conversation`] composes the timer with the state
/// machine and [`crate::core::ConversationConfig`] durations.
#[derive(Debug, Clone, Default)]
pub struct PhaseTimer {
    /// Meaning depends on the orchestrator's intent at start time:
    /// - PendingJoin: time the join was initiated.
    /// - Working: time the first approved proposal arrived
    ///   (drives the steward-inactivity timer).
    /// - Freezing: time the freeze window started.
    /// - Other states: `None`.
    started_at: Option<Instant>,
}

impl PhaseTimer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Anchor the timer at "now". Called by the orchestrator when entering
    /// a phase whose timeout matters (PendingJoin, Freezing, on first
    /// approved proposal in Working).
    pub fn start(&mut self) {
        self.started_at = Some(Instant::now());
    }

    /// Drop the anchor. Called by the orchestrator when leaving a
    /// time-bounded phase.
    pub fn clear(&mut self) {
        self.started_at = None;
    }

    pub fn started_at(&self) -> Option<Instant> {
        self.started_at
    }

    /// `false` when no anchor is set. Caller is responsible for state
    /// guarding and for choosing the right duration for the current phase.
    pub fn elapsed_since_anchor(&self, duration: Duration) -> bool {
        match self.started_at {
            Some(t) => Instant::now() >= t + duration,
            None => false,
        }
    }

    /// Test-only: overwrite the anchor with an explicit `Instant`. Lets
    /// timer-boundary tests synthesize an aged anchor without sleeping.
    #[cfg(test)]
    pub(crate) fn set_started_at_for_test(&mut self, anchor: Option<Instant>) {
        self.started_at = anchor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn unset_never_elapsed() {
        let pt = PhaseTimer::new();
        assert!(!pt.elapsed_since_anchor(Duration::from_secs(1)));
    }

    #[test]
    fn fresh_anchor_not_elapsed() {
        let mut pt = PhaseTimer::new();
        pt.start();
        assert!(!pt.elapsed_since_anchor(Duration::from_secs(60)));
    }

    #[test]
    fn elapsed_when_anchor_old_enough() {
        let mut pt = PhaseTimer::new();
        pt.started_at = Some(Instant::now() - Duration::from_secs(30));
        assert!(pt.elapsed_since_anchor(Duration::from_secs(1)));
    }

    #[test]
    fn clear_drops_anchor() {
        let mut pt = PhaseTimer::new();
        pt.start();
        assert!(pt.started_at().is_some());
        pt.clear();
        assert!(pt.started_at().is_none());
    }
}
