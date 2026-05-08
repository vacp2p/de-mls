//! App-side phase timer.
//!
//! Holds the wall-clock anchor (`started_at`) and the phase-anchor
//! `Duration` knobs the timer-driven helpers consult: commit inactivity,
//! freeze, recovery inactivity. Pure timer state — `GroupState` awareness
//! lives one layer up where the timer is composed with
//! [`crate::core::GroupStateMachine`].

use std::time::{Duration, Instant};

use crate::app::config::GroupConfig;

/// What a freeze-timeout poll returned.
#[derive(Debug, PartialEq)]
pub enum FreezeTimeoutStatus {
    NotFreezing,
    StillFreezing,
    /// A candidate was selected and applied.
    Applied,
    /// Timeout elapsed without a valid candidate. `has_proposals = true`
    /// means approved work existed at timeout (steward fault); `false` is
    /// just an empty epoch.
    TimedOut {
        has_proposals: bool,
    },
}

/// Wall-clock anchor + the phase-anchor durations the timer-driven
/// helpers consult. Pure timer state — `GroupState` is not stored here.
/// Composition with the state machine happens at the orchestration layer
/// (`GroupEntry` today; `SessionRunner` after `GroupEntry → core`).
#[derive(Debug, Clone)]
pub struct PhaseTimer {
    /// Meaning depends on the orchestrator's intent at start time:
    /// - PendingJoin: time the join was initiated.
    /// - Working: time the first approved proposal arrived
    ///   (drives the steward-inactivity timer).
    /// - Freezing: time the freeze window started.
    /// - Other states: `None`.
    started_at: Option<Instant>,
    /// RFC §Inactivity Timer #1 — "Commit inactivity" threshold.
    commit_inactivity_duration: Duration,
    freeze_duration: Duration,
    /// RFC §Inactivity Timer #2 — "Recovery inactivity" threshold; caller
    /// of `inactivity_elapsed` picks which duration to apply.
    recovery_inactivity_duration: Duration,
}

impl Default for PhaseTimer {
    fn default() -> Self {
        Self::with_default_config()
    }
}

impl PhaseTimer {
    pub fn with_default_config() -> Self {
        Self::with_config(&GroupConfig::default())
    }

    pub fn with_config(config: &GroupConfig) -> Self {
        Self {
            started_at: None,
            commit_inactivity_duration: config.commit_inactivity_duration,
            freeze_duration: config.freeze_duration,
            recovery_inactivity_duration: config.recovery_inactivity_duration,
        }
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

    pub fn commit_inactivity_duration(&self) -> Duration {
        self.commit_inactivity_duration
    }

    pub fn freeze_duration(&self) -> Duration {
        self.freeze_duration
    }

    pub fn recovery_inactivity_duration(&self) -> Duration {
        self.recovery_inactivity_duration
    }

    // Setters below are called by `User::on_group_sync` to apply the
    // steward's `TimingConfig`.

    pub fn set_commit_inactivity_duration(&mut self, value: Duration) {
        self.commit_inactivity_duration = value;
    }

    pub fn set_freeze_duration(&mut self, value: Duration) {
        self.freeze_duration = value;
    }

    pub fn set_recovery_inactivity_duration(&mut self, value: Duration) {
        self.recovery_inactivity_duration = value;
    }

    // ─────────────────────────── Pure timer queries ───────────────────────────

    /// `true` once 3× `commit_inactivity_duration` has elapsed since the
    /// anchor was set. Caller is responsible for state guarding (i.e.,
    /// only meaningful in PendingJoin).
    pub fn pending_join_elapsed(&self) -> bool {
        match self.started_at {
            // Pipeline: consensus (~15s) + commit_inactivity + freeze (≈ commit/2)
            // = ~1.5× commit-inactivity + consensus overhead. Use 3× for safety margin.
            Some(t) => Instant::now() >= t + self.commit_inactivity_duration * 3,
            None => false,
        }
    }

    /// `true` once `freeze_duration` has elapsed since the anchor was set.
    /// Caller is responsible for state guarding (i.e., only meaningful in
    /// Freezing).
    pub fn freeze_window_elapsed(&self) -> bool {
        match self.started_at {
            Some(t) => Instant::now() >= t + self.freeze_duration,
            None => false,
        }
    }

    /// `true` once `inactivity_duration` has elapsed since the anchor was
    /// set. Caller is responsible for state guarding (Working) and for
    /// having anchored the timer (`start`) at the first approved-proposal
    /// observation.
    pub fn inactivity_elapsed(&self, inactivity_duration: Duration) -> bool {
        match self.started_at {
            Some(t) => Instant::now() >= t + inactivity_duration,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_inactivity() -> Duration {
        Duration::from_secs(60)
    }

    fn short_inactivity() -> Duration {
        Duration::from_secs(5)
    }

    #[test]
    fn pending_join_not_elapsed_when_unset() {
        let pt = PhaseTimer::with_default_config();
        assert!(!pt.pending_join_elapsed());
    }

    #[test]
    fn pending_join_elapses_after_three_commit_inactivity_windows() {
        let config = GroupConfig::default();
        let mut pt = PhaseTimer::with_config(&config);
        pt.start();
        assert!(!pt.pending_join_elapsed());

        let past = config.commit_inactivity_duration * 3 + Duration::from_secs(1);
        pt.started_at = Some(Instant::now() - past);
        assert!(pt.pending_join_elapsed());
    }

    #[test]
    fn freeze_window_not_elapsed_fresh() {
        let mut pt = PhaseTimer::with_default_config();
        pt.start();
        assert!(!pt.freeze_window_elapsed());
    }

    #[test]
    fn freeze_window_elapsed_when_anchor_old() {
        let mut pt = PhaseTimer::with_default_config();
        pt.started_at = Some(Instant::now() - Duration::from_secs(30));
        assert!(pt.freeze_window_elapsed());
    }

    #[test]
    fn inactivity_uses_caller_supplied_duration() {
        let mut pt = PhaseTimer::with_default_config();
        pt.started_at = Some(Instant::now() - Duration::from_millis(100));
        assert!(!pt.inactivity_elapsed(long_inactivity()));
        assert!(pt.inactivity_elapsed(Duration::from_millis(50)));
    }

    #[test]
    fn inactivity_returns_false_when_unset() {
        let pt = PhaseTimer::with_default_config();
        assert!(!pt.inactivity_elapsed(Duration::from_millis(50)));
    }

    #[test]
    fn recovery_inactivity_threaded_from_config() {
        let config = GroupConfig {
            commit_inactivity_duration: long_inactivity(),
            recovery_inactivity_duration: short_inactivity(),
            ..GroupConfig::default()
        };
        let pt = PhaseTimer::with_config(&config);
        assert_eq!(pt.commit_inactivity_duration(), long_inactivity());
        assert_eq!(pt.recovery_inactivity_duration(), short_inactivity());
    }
}
