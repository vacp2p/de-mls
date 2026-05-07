//! App-side phase timer.
//!
//! Holds the wall-clock anchor (`started_at`) and the `Duration` knobs
//! the timer-driven helpers consult: commit inactivity, freeze, recovery
//! inactivity, proposal expiration, consensus timeout, voting delays.
//! State transitions and the state enum live in
//! [`crate::core::GroupStateMachine`]; `GroupEntry` composes the two and
//! drives them through coordinator methods.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::info;

use crate::{
    app::config::GroupConfig,
    core::{GroupState, ProposalKind},
};

/// Notifies the integrator when a group transitions between [`GroupState`]
/// variants. Fired from the app layer.
#[async_trait]
pub trait StateChangeHandler: Send + Sync {
    async fn on_state_changed(&self, group_name: &str, state: GroupState);
}

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

/// App-side phase timer: a wall-clock anchor plus the duration knobs the
/// timer-driven helpers consult. State queries take the current
/// [`GroupState`] as a parameter; [`crate::app::User`] composes them
/// with [`crate::core::GroupStateMachine`] through `GroupEntry`.
#[derive(Debug, Clone)]
pub struct PhaseTimer {
    /// Meaning depends on the current state (set by the coordinator):
    /// - `PendingJoin`: time the join was initiated.
    /// - `Working`: time the first approved proposal arrived (drives the
    ///   steward-inactivity timer).
    /// - `Freezing`: time the freeze window started.
    /// - Other states: `None`.
    started_at: Option<Instant>,
    /// RFC §Inactivity Timer #1 — "Commit inactivity" threshold.
    commit_inactivity_duration: Duration,
    freeze_duration: Duration,
    /// RFC §Inactivity Timer #2 — "Recovery inactivity" threshold; caller
    /// of `poll_steward_inactivity` picks which duration to apply.
    recovery_inactivity_duration: Duration,
    /// Voting-proposal lifetime (RFC §Creating Voting Proposal).
    proposal_expiration: Duration,
    /// Library deadline per consensus session. Mismatched values across
    /// nodes split outcomes.
    consensus_timeout: Duration,
    /// Per-member window before auto-vote fires. Local-only.
    voting_delay: Duration,
    /// Auto-vote delay for steward-election proposals. Local-only.
    election_voting_delay: Duration,
}

impl Default for PhaseTimer {
    fn default() -> Self {
        Self::with_default_config()
    }
}

impl PhaseTimer {
    pub fn with_default_config() -> Self {
        Self::with_config(GroupConfig::default())
    }

    pub fn with_config(config: GroupConfig) -> Self {
        Self {
            started_at: None,
            commit_inactivity_duration: config.commit_inactivity_duration,
            freeze_duration: config.freeze_duration,
            recovery_inactivity_duration: config.recovery_inactivity_duration,
            proposal_expiration: config.proposal_expiration,
            consensus_timeout: config.consensus_timeout,
            voting_delay: config.voting_delay,
            election_voting_delay: config.election_voting_delay,
        }
    }

    /// Anchor the timer at "now". Called by the coordinator when entering
    /// a phase whose timeout matters (PendingJoin, Freezing, on first
    /// approved proposal in Working).
    pub fn start(&mut self) {
        self.started_at = Some(Instant::now());
    }

    /// Drop the anchor. Called by the coordinator when leaving a
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

    pub fn proposal_expiration(&self) -> Duration {
        self.proposal_expiration
    }

    pub fn consensus_timeout(&self) -> Duration {
        self.consensus_timeout
    }

    /// Steward-election proposals use the shorter `election_voting_delay`
    /// for fast recovery convergence.
    pub fn voting_delay_for(&self, kind: ProposalKind) -> Duration {
        if kind.is_steward_election() {
            self.election_voting_delay
        } else {
            self.voting_delay
        }
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

    pub fn set_proposal_expiration(&mut self, value: Duration) {
        self.proposal_expiration = value;
    }

    pub fn set_consensus_timeout(&mut self, value: Duration) {
        self.consensus_timeout = value;
    }

    // ─────────────────────────── State-aware queries ───────────────────────────

    /// `true` once 3× `commit_inactivity_duration` has passed in
    /// `PendingJoin` without a welcome — the join attempt is abandoned
    /// and local state torn down.
    pub fn is_pending_join_expired(&self, state: GroupState) -> bool {
        if state != GroupState::PendingJoin {
            return false;
        }

        if let Some(started_at) = self.started_at {
            // Pipeline: consensus (~15s) + commit_inactivity + freeze (≈ commit/2)
            // = ~1.5× commit-inactivity + consensus overhead. Use 3× for safety margin.
            let max_wait = self.commit_inactivity_duration * 3;
            if Instant::now() >= started_at + max_wait {
                return true;
            }
        }

        false
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub fn is_freeze_timed_out(&self, state: GroupState) -> bool {
        if state != GroupState::Freezing {
            return false;
        }

        if let Some(started_at) = self.started_at {
            return Instant::now() >= started_at + self.freeze_duration;
        }

        false
    }

    /// Polls the steward-inactivity timer. Returns `true` when the
    /// inactivity window has elapsed and the caller should transition to
    /// `Freezing`. Self-starts the timer on the first call with approved
    /// work; returns `false` outside `Working` or with no approved work.
    ///
    /// - The timer anchors on the *first* approved proposal — a burst of
    ///   approvals doesn't reset it.
    /// - The coordinator's `start_working` clears the anchor, so the next
    ///   tick with leftover approved work starts a fresh window (matters
    ///   for auto-approved self-leaves that survive a reelection round).
    /// - `inactivity_duration` is supplied by the caller (long during
    ///   normal operation, short during recovery).
    pub fn poll_steward_inactivity(
        &mut self,
        state: GroupState,
        approved_proposals_count: usize,
        inactivity_duration: Duration,
    ) -> bool {
        if state != GroupState::Working || approved_proposals_count == 0 {
            return false;
        }

        let first_approved = match self.started_at {
            Some(t) => t,
            None => {
                self.started_at = Some(Instant::now());
                info!(
                    approved = approved_proposals_count,
                    inactivity_ms = inactivity_duration.as_millis() as u64,
                    "inactivity timer started"
                );
                return false;
            }
        };

        if Instant::now() < first_approved + inactivity_duration {
            return false;
        }

        info!(
            inactivity_ms = inactivity_duration.as_millis() as u64,
            approved = approved_proposals_count,
            "inactivity window elapsed, entering freeze"
        );
        true
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
    fn pending_join_not_expired_when_unset() {
        let pt = PhaseTimer::with_default_config();
        assert!(!pt.is_pending_join_expired(GroupState::PendingJoin));
    }

    #[test]
    fn pending_join_expires_after_three_commit_inactivity_windows() {
        let config = GroupConfig::default();
        let mut pt = PhaseTimer::with_config(config.clone());
        pt.start();
        assert!(!pt.is_pending_join_expired(GroupState::PendingJoin));

        // Backdate past the 3× commit_inactivity_duration expiration threshold.
        let past = config.commit_inactivity_duration * 3 + Duration::from_secs(1);
        pt.started_at = Some(Instant::now() - past);
        assert!(pt.is_pending_join_expired(GroupState::PendingJoin));
    }

    #[test]
    fn pending_join_check_only_in_pending_join() {
        let mut pt = PhaseTimer::with_default_config();
        pt.start();
        pt.started_at = Some(Instant::now() - Duration::from_secs(100_000));
        // Even with a stale anchor, non-PendingJoin states never expire.
        assert!(!pt.is_pending_join_expired(GroupState::Working));
    }

    #[test]
    fn freeze_timeout_only_in_freezing() {
        let pt = PhaseTimer::with_default_config();
        assert!(!pt.is_freeze_timed_out(GroupState::Working));
    }

    #[test]
    fn freeze_timeout_fresh_anchor_not_timed_out() {
        let mut pt = PhaseTimer::with_default_config();
        pt.start();
        assert!(!pt.is_freeze_timed_out(GroupState::Freezing));
    }

    #[test]
    fn freeze_timeout_expired() {
        let mut pt = PhaseTimer::with_default_config();
        pt.started_at = Some(Instant::now() - Duration::from_secs(30));
        assert!(pt.is_freeze_timed_out(GroupState::Freezing));
    }

    #[test]
    fn inactivity_self_starts_on_first_check_with_approved() {
        let mut pt = PhaseTimer::with_default_config();
        assert!(pt.started_at.is_none());

        // First call with approved work: timer starts, no transition yet.
        assert!(!pt.poll_steward_inactivity(GroupState::Working, 1, long_inactivity()));
        assert!(pt.started_at.is_some());
    }

    #[test]
    fn inactivity_not_restarted_while_running() {
        let mut pt = PhaseTimer::with_default_config();
        pt.poll_steward_inactivity(GroupState::Working, 1, long_inactivity());
        let first_time = pt.started_at.unwrap();

        std::thread::sleep(Duration::from_millis(5));

        // Same-or-more approved → same timer.
        pt.poll_steward_inactivity(GroupState::Working, 2, long_inactivity());
        assert_eq!(pt.started_at.unwrap(), first_time);
    }

    #[test]
    fn inactivity_triggers_freeze_signal() {
        let mut pt = PhaseTimer::with_default_config();
        pt.started_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(pt.poll_steward_inactivity(GroupState::Working, 1, Duration::from_millis(50)));
    }

    #[test]
    fn inactivity_uses_caller_supplied_duration() {
        let mut pt = PhaseTimer::with_default_config();
        pt.started_at = Some(Instant::now() - Duration::from_millis(100));
        assert!(!pt.poll_steward_inactivity(GroupState::Working, 1, long_inactivity()));
        assert!(pt.poll_steward_inactivity(GroupState::Working, 1, Duration::from_millis(50)));
    }

    #[test]
    fn inactivity_skips_outside_working() {
        let mut pt = PhaseTimer::with_default_config();
        pt.started_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(!pt.poll_steward_inactivity(GroupState::Freezing, 1, Duration::from_millis(50)));
    }

    #[test]
    fn recovery_inactivity_threaded_from_config() {
        let config = GroupConfig {
            commit_inactivity_duration: long_inactivity(),
            recovery_inactivity_duration: short_inactivity(),
            ..GroupConfig::default()
        };
        let pt = PhaseTimer::with_config(config);
        assert_eq!(pt.commit_inactivity_duration(), long_inactivity());
        assert_eq!(pt.recovery_inactivity_duration(), short_inactivity());
    }

    /// `voting_delay_for` dispatches on proposal kind: steward-election
    /// proposals get the shorter `election_voting_delay`, others get
    /// `voting_delay`.
    #[test]
    fn voting_delay_dispatch_on_proposal_kind() {
        let config = GroupConfig {
            voting_delay: Duration::from_secs(7),
            election_voting_delay: Duration::from_secs(3),
            ..GroupConfig::default()
        };
        let pt = PhaseTimer::with_config(config);
        assert_eq!(
            pt.voting_delay_for(ProposalKind::Commit),
            Duration::from_secs(7)
        );
        assert_eq!(
            pt.voting_delay_for(ProposalKind::StewardElection),
            Duration::from_secs(3)
        );
    }
}
