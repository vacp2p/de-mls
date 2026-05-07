//! App-side wrapper around [`crate::core::GroupStateMachine`].
//!
//! Adds the timer-driven behaviour the core SM intentionally lacks:
//! a `started_at` (`Instant`), the `Duration` knobs (epoch, freeze,
//! retry inactivity, proposal expiration, consensus timeout, voting
//! delays), and the helpers that combine state + duration to answer
//! "is this phase timed out?". State transitions delegate to the core
//! SM and additionally maintain `started_at`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use tracing::info;

use crate::{
    app::config::GroupConfig,
    core::{GroupStateMachine as CoreStateMachine, ProposalKind},
};

// `GroupState` re-exported so `crate::app::GroupState` callsites work
// unchanged. The enum itself lives in core.
pub use crate::core::GroupState;

/// Notifies the integrator when a group transitions between [`GroupState`]
/// variants. Fires from the app layer only — core never calls it.
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

/// App-side state machine: the passive core SM plus a phase timer and
/// the duration knobs the timer-driven helpers consult.
///
/// Transitions delegate to the core SM and additionally maintain the
/// phase timer. Timer queries (`is_freeze_timed_out`, etc.) live here
/// because they need wall-clock time.
#[derive(Debug, Clone)]
pub struct PhaseTimer {
    inner: CoreStateMachine,
    /// Meaning depends on the current core state:
    /// - `PendingJoin`: time the join was initiated.
    /// - `Working`: time the first approved proposal arrived (drives the
    ///   steward-inactivity timer).
    /// - `Freezing`: time the freeze window started.
    /// - Other states: `None`.
    started_at: Option<Instant>,
    epoch_duration: Duration,
    freeze_duration: Duration,
    /// Short inactivity window used during recovery; caller of
    /// `check_steward_inactivity` picks which duration to apply.
    retry_inactivity_duration: Duration,
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
        Self::new_as_member()
    }
}

impl PhaseTimer {
    pub fn new_as_member() -> Self {
        Self::new_as_member_with_config(GroupConfig::default())
    }

    pub fn new_as_member_with_config(config: GroupConfig) -> Self {
        Self {
            inner: CoreStateMachine::new_as_member(),
            started_at: None,
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration,
            retry_inactivity_duration: config.retry_inactivity_duration,
            proposal_expiration: config.proposal_expiration,
            consensus_timeout: config.consensus_timeout,
            voting_delay: config.voting_delay,
            election_voting_delay: config.election_voting_delay,
        }
    }

    pub fn new_as_pending_join_with_config(config: GroupConfig) -> Self {
        Self {
            inner: CoreStateMachine::new_as_pending_join(),
            started_at: Some(Instant::now()),
            epoch_duration: config.epoch_duration,
            freeze_duration: config.freeze_duration,
            retry_inactivity_duration: config.retry_inactivity_duration,
            proposal_expiration: config.proposal_expiration,
            consensus_timeout: config.consensus_timeout,
            voting_delay: config.voting_delay,
            election_voting_delay: config.election_voting_delay,
        }
    }

    pub fn current_state(&self) -> GroupState {
        self.inner.current_state()
    }

    pub fn epoch_duration(&self) -> Duration {
        self.epoch_duration
    }

    pub fn freeze_duration(&self) -> Duration {
        self.freeze_duration
    }

    pub fn retry_inactivity_duration(&self) -> Duration {
        self.retry_inactivity_duration
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

    pub fn set_epoch_duration(&mut self, value: Duration) {
        self.epoch_duration = value;
    }

    pub fn set_freeze_duration(&mut self, value: Duration) {
        self.freeze_duration = value;
    }

    pub fn set_retry_inactivity_duration(&mut self, value: Duration) {
        self.retry_inactivity_duration = value;
    }

    pub fn set_proposal_expiration(&mut self, value: Duration) {
        self.proposal_expiration = value;
    }

    pub fn set_consensus_timeout(&mut self, value: Duration) {
        self.consensus_timeout = value;
    }

    // ─────────────────────────── State transitions ───────────────────────────

    pub fn start_working(&mut self) {
        self.inner.start_working();
        self.started_at = None;
        info!(state = "Working", "state transition");
    }

    pub fn start_freezing(&mut self) {
        self.inner.start_freezing();
        self.started_at = Some(Instant::now());
        info!(state = "Freezing", "state transition");
    }

    /// Bypass the inactivity timer and enter Freezing immediately. Returns
    /// `true` on transition (only fires from `Working` or `Reelection`).
    pub fn force_freezing(&mut self) -> bool {
        if self.inner.force_freezing() {
            self.started_at = Some(Instant::now());
            info!(state = "Freezing", "state transition (forced)");
            true
        } else {
            false
        }
    }

    pub fn start_selection(&mut self) {
        self.inner.start_selection();
        info!(state = "Selection", "state transition");
    }

    pub fn start_reelection(&mut self) {
        self.inner.start_reelection();
        self.started_at = None;
        info!(state = "Reelection", "state transition");
    }

    /// Caller must ensure a valid transition. `User::leave_group` handles
    /// the PendingJoin and already-Leaving cases separately.
    pub fn start_leaving(&mut self) {
        self.inner.start_leaving();
        info!(state = "Leaving", "state transition");
    }

    // ─────────────────────────── Pending Join ───────────────────────────

    /// `true` once 3× epoch-duration has passed in `PendingJoin` without a
    /// welcome — the join attempt is abandoned and local state torn down.
    pub fn is_pending_join_expired(&self) -> bool {
        if self.current_state() != GroupState::PendingJoin {
            return false;
        }

        if let Some(started_at) = self.started_at {
            // Pipeline: consensus (~15s) + epoch_duration + freeze_duration (epoch/2)
            // = ~1.5× epoch + consensus overhead. Use 3× epoch for safety margin.
            let max_wait = self.epoch_duration * 3;
            if Instant::now() >= started_at + max_wait {
                return true;
            }
        }

        false
    }

    // ─────────────────────────── Freeze Timeout ───────────────────────────

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub fn is_freeze_timed_out(&self) -> bool {
        if self.current_state() != GroupState::Freezing {
            return false;
        }

        if let Some(started_at) = self.started_at {
            return Instant::now() >= started_at + self.freeze_duration;
        }

        false
    }
    // ─────────────────────────── Proposal Timer (Member Inactivity) ───────────────────────────

    pub fn clear_proposal_timer(&mut self) {
        self.started_at = None;
    }

    /// Drives the "steward waited too long to commit" transition into
    /// `Freezing`. Call each poll tick; returns `true` exactly on the tick
    /// that transitions.
    ///
    /// - The timer anchors on the *first* approved proposal — a burst of
    ///   approvals doesn't reset it.
    /// - `start_working` clears the timer, so the next tick with leftover
    ///   approved work starts a fresh window (matters for auto-approved
    ///   self-leaves that survive a reelection round).
    /// - `inactivity_duration` is supplied by the caller (long during
    ///   normal operation, short during recovery).
    /// - No-op outside `Working`.
    pub fn check_steward_inactivity(
        &mut self,
        approved_proposals_count: usize,
        inactivity_duration: Duration,
    ) -> bool {
        if self.current_state() != GroupState::Working || approved_proposals_count == 0 {
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

        self.start_freezing();
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

    #[test]
    fn test_pending_join_timeout() {
        let config = GroupConfig::default();
        let mut state_machine = PhaseTimer::new_as_pending_join_with_config(config.clone());
        assert!(!state_machine.is_pending_join_expired());

        // Backdate past the 3× epoch_duration expiration threshold.
        let past = config.epoch_duration * 3 + Duration::from_secs(1);
        state_machine.started_at = Some(Instant::now() - past);
        assert!(state_machine.is_pending_join_expired());
    }

    #[test]
    fn test_pending_join_not_expired_when_working() {
        // is_pending_join_expired is meaningful only in PendingJoin.
        let state_machine = PhaseTimer::new_as_member();
        assert!(!state_machine.is_pending_join_expired());
    }

    #[test]
    fn test_freeze_timeout_not_in_freezing() {
        let state_machine = PhaseTimer::new_as_member();
        // Not in Freezing → not timed out
        assert!(!state_machine.is_freeze_timed_out());
    }

    #[test]
    fn test_freeze_timeout_fresh_freezing() {
        let mut state_machine = PhaseTimer::new_as_member();
        state_machine.start_freezing();
        // Just entered Freezing → not timed out yet
        assert!(!state_machine.is_freeze_timed_out());
    }

    #[test]
    fn test_freeze_timeout_expired() {
        let mut state_machine = PhaseTimer::new_as_member();
        state_machine.start_freezing();
        // Backdate phase start well past freeze duration.
        state_machine.started_at = Some(Instant::now() - Duration::from_secs(30));
        assert!(state_machine.is_freeze_timed_out());
    }

    #[test]
    fn test_freeze_timeout_cleared_on_working() {
        let mut state_machine = PhaseTimer::new_as_member();
        state_machine.start_freezing();
        assert!(state_machine.started_at.is_some());

        state_machine.start_working();
        assert!(state_machine.started_at.is_none());
        assert!(!state_machine.is_freeze_timed_out());
    }

    // ─────────────────────────── Proposal Timer Tests ───────────────────────────

    fn long_inactivity() -> Duration {
        Duration::from_secs(60)
    }

    fn short_inactivity() -> Duration {
        Duration::from_secs(5)
    }

    #[test]
    fn test_inactivity_timer_self_starts_on_first_check_with_approved() {
        let mut sm = PhaseTimer::new_as_member();
        assert!(sm.started_at.is_none());

        // First call with approved work: timer starts, no transition yet.
        assert!(!sm.check_steward_inactivity(1, long_inactivity()));
        assert!(sm.started_at.is_some());
    }

    #[test]
    fn test_inactivity_timer_not_restarted_while_running() {
        let mut sm = PhaseTimer::new_as_member();
        sm.check_steward_inactivity(1, long_inactivity());
        let first_time = sm.started_at.unwrap();

        std::thread::sleep(Duration::from_millis(5));

        // Same-or-more approved → same timer.
        sm.check_steward_inactivity(2, long_inactivity());
        assert_eq!(sm.started_at.unwrap(), first_time);
    }

    #[test]
    fn test_steward_inactivity_triggers_freezing() {
        let mut sm = PhaseTimer::new_as_member();
        sm.started_at = Some(Instant::now() - Duration::from_secs(1));

        assert!(sm.check_steward_inactivity(1, Duration::from_millis(50)));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    #[test]
    fn test_check_inactivity_uses_caller_supplied_duration() {
        let mut sm = PhaseTimer::new_as_member();
        sm.started_at = Some(Instant::now() - Duration::from_millis(100));
        assert!(!sm.check_steward_inactivity(1, long_inactivity()));
        assert_eq!(sm.current_state(), GroupState::Working);

        assert!(sm.check_steward_inactivity(1, Duration::from_millis(50)));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    #[test]
    fn test_retry_inactivity_duration_threaded_from_config() {
        let config = GroupConfig {
            epoch_duration: long_inactivity(),
            retry_inactivity_duration: short_inactivity(),
            ..GroupConfig::default()
        };
        let sm = PhaseTimer::new_as_member_with_config(config);
        assert_eq!(sm.epoch_duration(), long_inactivity());
        assert_eq!(sm.retry_inactivity_duration(), short_inactivity());
    }

    /// `voting_delay_for` dispatches on proposal kind: steward-election
    /// proposals get the shorter `election_voting_delay`, others get
    /// `voting_delay`.
    #[test]
    fn test_voting_delay_dispatch_on_proposal_kind() {
        let config = GroupConfig {
            voting_delay: Duration::from_secs(7),
            election_voting_delay: Duration::from_secs(3),
            ..GroupConfig::default()
        };
        let sm = PhaseTimer::new_as_member_with_config(config);
        assert_eq!(
            sm.voting_delay_for(ProposalKind::Commit),
            Duration::from_secs(7)
        );
        assert_eq!(
            sm.voting_delay_for(ProposalKind::StewardElection),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn test_force_freezing_sets_started_at() {
        let mut sm = PhaseTimer::new_as_member();
        assert!(sm.force_freezing());
        assert_eq!(sm.current_state(), GroupState::Freezing);
        assert!(sm.started_at.is_some());
    }

    #[test]
    fn test_steward_inactivity_skips_if_already_freezing() {
        let mut sm = PhaseTimer::new_as_member();
        sm.started_at = Some(Instant::now() - Duration::from_secs(1));
        sm.start_freezing();

        // Already in Freezing — should not re-trigger
        assert!(!sm.check_steward_inactivity(1, Duration::from_millis(50)));
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }
}
