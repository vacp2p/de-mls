//! Per-conversation timing and policy configuration.

use std::time::Duration;

use crate::StewardListConfig;
use crate::error::ConversationError;
use crate::peer_scoring::ScoringConfig;
use crate::protos::de_mls::messages::v1::TimingConfig;

/// Window that batches approved proposals from the first one; at its end every
/// steward mints a commit candidate (RFC §Inactivity Timer #1).
pub const DEFAULT_COMMIT_BATCH_WINDOW: Duration = Duration::from_secs(60);

/// Lifetime of a voting proposal before it expires unvoted
/// (RFC §Creating Voting Proposal).
pub const DEFAULT_PROPOSAL_EXPIRATION: Duration = Duration::from_secs(600);

/// Library deadline for a single consensus session — bounds how long a
/// vote can stay open. MUST be `> voting_delay`.
pub const DEFAULT_CONSENSUS_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a backup steward waits for a silent epoch steward before covering
/// its propose / sync-resend (RFC §Inactivity Timer #3).
pub const DEFAULT_BACKUP_TAKEOVER_WINDOW: Duration = Duration::from_secs(20);

/// Per-member window to cast a manual vote before the app auto-votes
/// using `liveness_criteria_yes`. MUST be `< consensus_timeout`.
pub const DEFAULT_VOTING_DELAY: Duration = Duration::from_secs(10);

pub const DEFAULT_LIVENESS_CRITERIA_YES: bool = true;

pub const DEFAULT_PENDING_UPDATE_MAX_EPOCHS: u32 = 3;

/// Max consensus sessions kept per conversation before the library evicts the
/// oldest. Resolved sessions are freed on outcome, so this bounds concurrent
/// in-flight proposals; kept below the resolved-proposal cache capacity.
pub const DEFAULT_MAX_CONSENSUS_SESSIONS: usize = 128;

/// Cap on MLS proposals the steward packs into one commit batch — bounds
/// runaway growth when a busy epoch accumulates approved work.
pub const DEFAULT_COMMIT_BATCH_MAX: usize = 50;

/// Default number of recent commit / welcome hashes kept for duplicate
/// detection (see [`EngineConfig::dedup_window`]).
pub const DEFAULT_DEDUP_WINDOW: usize = 10;

/// Default unanswered sync-request rounds before the conversation reports it:
/// three tries across the answer latency. `0` never reports. See
/// [`EngineConfig::unanswered_sync_rounds`].
pub const DEFAULT_UNANSWERED_SYNC_ROUNDS: u32 = 3;

/// Per-conversation timing and policy config, fixed at conversation creation.
/// Peer scoring keeps its own config on the [`PeerScoringService`](crate::PeerScoringService)
/// the integrator builds; the steward-list bounds live here (the list is
/// library-owned) as the nested [`steward_list`](Self::steward_list).
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// How long approved proposals are batched before every steward mints a
    /// commit candidate (RFC §Inactivity Timer #1).
    pub commit_batch_window: Duration,
    /// How long the freeze collects commit candidates before one is
    /// deterministically selected (RFC §Freezing).
    pub freeze_duration: Duration,
    /// How long a backup steward waits for a silent epoch steward (to propose a
    /// buffered update, or answer a sync-resend) before covering it
    /// (RFC §Inactivity Timer #3).
    pub backup_takeover_window: Duration,
    /// How long a proposal stays open before it expires unvoted
    /// (RFC §Creating Voting Proposal).
    pub proposal_expiration: Duration,
    /// How long a vote session waits for ⌈2n/3⌉ real votes before the
    /// silent-vote fallback (RFC §Voting).
    pub consensus_timeout: Duration,
    /// How long a member waits before auto-casting `liveness_criteria_yes`.
    /// Must be `< consensus_timeout`, or the auto-vote never fires before the
    /// session closes (enforced by [`validate`](Self::validate)).
    pub voting_delay: Duration,
    /// Max age (in epochs) of a buffered membership update. An entry first
    /// seen at epoch `E` is dropped once `current_epoch - E` exceeds this
    /// value (so it survives epochs `E..=E + max_age` inclusive).
    pub pending_update_max_epochs: u32,
    /// Whether silent voters count as YES at `consensus_timeout` (RFC
    /// §Creating Voting Proposal). See [`DEFAULT_LIVENESS_CRITERIA_YES`].
    /// Also used by the auto-vote timer as the cast value.
    pub liveness_criteria_yes: bool,
    /// Max consensus sessions retained per conversation before the oldest is
    /// evicted. See [`DEFAULT_MAX_CONSENSUS_SESSIONS`].
    pub max_consensus_sessions: usize,
    /// Max MLS proposals the steward packs into one commit batch. See
    /// [`DEFAULT_COMMIT_BATCH_MAX`].
    pub commit_batch_max: usize,
    /// How many recent commit / welcome hashes each dedup window retains
    /// (duplicate-candidate and duplicate-welcome-broadcast detection). See
    /// [`DEFAULT_DEDUP_WINDOW`].
    pub dedup_window: usize,
    /// How many sync requests may go unanswered before the conversation emits
    /// [`crate::engine::Event::SyncUnanswered`]. Rounds are one
    /// `backup_takeover_window` apart. `0` never emits it; requesting continues
    /// either way. See [`DEFAULT_UNANSWERED_SYNC_ROUNDS`].
    pub unanswered_sync_rounds: u32,
    /// Steward-list size bounds (`sn_min` / `sn_max`). The list itself is
    /// library-owned; this is the only steward knob the integrator sets.
    pub steward_list: StewardListConfig,
    /// Peer-scoring policy: the starting score and the removal threshold.
    pub scoring: ScoringConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            commit_batch_window: DEFAULT_COMMIT_BATCH_WINDOW,
            freeze_duration: DEFAULT_COMMIT_BATCH_WINDOW / 2,
            backup_takeover_window: DEFAULT_BACKUP_TAKEOVER_WINDOW,
            proposal_expiration: DEFAULT_PROPOSAL_EXPIRATION,
            consensus_timeout: DEFAULT_CONSENSUS_TIMEOUT,
            pending_update_max_epochs: DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
            voting_delay: DEFAULT_VOTING_DELAY,
            liveness_criteria_yes: DEFAULT_LIVENESS_CRITERIA_YES,
            max_consensus_sessions: DEFAULT_MAX_CONSENSUS_SESSIONS,
            commit_batch_max: DEFAULT_COMMIT_BATCH_MAX,
            dedup_window: DEFAULT_DEDUP_WINDOW,
            unanswered_sync_rounds: DEFAULT_UNANSWERED_SYNC_ROUNDS,
            steward_list: StewardListConfig::default(),
            scoring: ScoringConfig::default(),
        }
    }
}

impl EngineConfig {
    /// Overwrite the duration fields from a wire [`TimingConfig`]. Used on
    /// the joiner side when applying `ConversationSync`. Non-timing fields
    /// (`liveness_criteria_yes`, `pending_update_max_epochs`) stay untouched.
    pub fn apply_timing(&mut self, timing: &TimingConfig) {
        apply_nonzero_ms(&mut self.commit_batch_window, timing.commit_batch_window_ms);
        apply_nonzero_ms(&mut self.freeze_duration, timing.freeze_duration_ms);
        apply_nonzero_ms(
            &mut self.backup_takeover_window,
            timing.backup_takeover_window_ms,
        );
        apply_nonzero_ms(&mut self.proposal_expiration, timing.proposal_expiration_ms);
        apply_nonzero_ms(&mut self.consensus_timeout, timing.consensus_timeout_ms);
    }

    /// Reject a config whose timers can't hold together. Enforces only the
    /// relations the library's own logic requires — not how any window relates
    /// to the network delay Δ, which the library can't know. Sizing each window
    /// above Δ is the integrator's call; a window below it degrades (e.g. two
    /// stewards forking one add) rather than failing here.
    pub fn validate(&self) -> Result<(), ConversationError> {
        self.scoring.validate()?;
        // The auto-vote fires at `voting_delay`; the session resolves at
        // `consensus_timeout`. If the delay isn't shorter, the auto-vote never
        // fires before the session closes.
        if self.voting_delay >= self.consensus_timeout {
            return Err(ConversationError::InvalidConfig(
                "voting_delay must be < consensus_timeout".to_owned(),
            ));
        }
        // A proposal must outlive its own voting session, or it expires before
        // the vote can resolve.
        if self.proposal_expiration <= self.consensus_timeout {
            return Err(ConversationError::InvalidConfig(
                "proposal_expiration must be > consensus_timeout".to_owned(),
            ));
        }
        // A zero collection window finalizes on whatever is in hand — no freeze.
        if self.freeze_duration.is_zero() {
            return Err(ConversationError::InvalidConfig(
                "freeze_duration must be non-zero".to_owned(),
            ));
        }
        if self.steward_list.sn_min < 1 || self.steward_list.sn_min > self.steward_list.sn_max {
            return Err(ConversationError::InvalidConfig(
                "steward_list requires 1 <= sn_min <= sn_max".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Overwrite `field` with `wire_ms` unless it is zero.
/// A zero wire duration would make its timer fire immediately (a
/// malformed-sync DoS); treat zero as "unset" and keep the local value.
fn apply_nonzero_ms(field: &mut Duration, wire_ms: u64) {
    if wire_ms != 0 {
        *field = Duration::from_millis(wire_ms);
    }
}

/// Build the wire [`TimingConfig`] from an [`EngineConfig`]. Used on
/// the steward side when sending `ConversationSync` to joiners.
impl From<&EngineConfig> for TimingConfig {
    fn from(config: &EngineConfig) -> Self {
        Self {
            commit_batch_window_ms: config.commit_batch_window.as_millis() as u64,
            freeze_duration_ms: config.freeze_duration.as_millis() as u64,
            backup_takeover_window_ms: config.backup_takeover_window.as_millis() as u64,
            proposal_expiration_ms: config.proposal_expiration.as_millis() as u64,
            consensus_timeout_ms: config.consensus_timeout.as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_config_round_trip() {
        let original = EngineConfig {
            commit_batch_window: Duration::from_millis(100),
            freeze_duration: Duration::from_millis(200),
            backup_takeover_window: Duration::from_millis(600),
            proposal_expiration: Duration::from_millis(400),
            consensus_timeout: Duration::from_millis(500),
            ..EngineConfig::default()
        };
        let timing = TimingConfig::from(&original);
        let mut applied = EngineConfig::default();
        applied.apply_timing(&timing);
        assert_eq!(applied.commit_batch_window, Duration::from_millis(100));
        assert_eq!(applied.freeze_duration, Duration::from_millis(200));
        assert_eq!(applied.proposal_expiration, Duration::from_millis(400));
        assert_eq!(applied.consensus_timeout, Duration::from_millis(500));
        assert_eq!(applied.backup_takeover_window, Duration::from_millis(600));
    }

    #[test]
    fn apply_timing_ignores_zero_durations() {
        let mut config = EngineConfig {
            consensus_timeout: Duration::from_secs(30),
            commit_batch_window: Duration::from_secs(60),
            ..EngineConfig::default()
        };
        let timing = TimingConfig {
            consensus_timeout_ms: 0,
            commit_batch_window_ms: 0,
            freeze_duration_ms: 250,
            proposal_expiration_ms: 0,
            backup_takeover_window_ms: 0,
        };
        config.apply_timing(&timing);
        // Zero fields keep their prior values.
        assert_eq!(config.consensus_timeout, Duration::from_secs(30));
        assert_eq!(config.commit_batch_window, Duration::from_secs(60));
        // Non-zero field is applied.
        assert_eq!(config.freeze_duration, Duration::from_millis(250));
    }

    #[test]
    fn default_config_is_valid() {
        assert!(EngineConfig::default().validate().is_ok());
    }

    #[test]
    fn millisecond_test_config_is_valid() {
        let config = EngineConfig {
            voting_delay: Duration::from_millis(50),
            consensus_timeout: Duration::from_millis(250),
            commit_batch_window: Duration::from_millis(500),
            freeze_duration: Duration::from_millis(500),
            proposal_expiration: Duration::from_millis(4000),
            ..EngineConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_voting_delay_not_below_consensus_timeout() {
        let config = EngineConfig {
            voting_delay: Duration::from_secs(30),
            consensus_timeout: Duration::from_secs(30),
            ..EngineConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(ConversationError::InvalidConfig(_))
        ));
    }

    #[test]
    fn validate_allows_consensus_timeout_above_commit_batch_window() {
        // Voting and batching are sequential phases, so a consensus_timeout
        // larger than commit_batch_window is a valid (if less-batched) choice.
        let config = EngineConfig {
            voting_delay: Duration::from_millis(30),
            consensus_timeout: Duration::from_millis(150),
            commit_batch_window: Duration::from_millis(50),
            ..EngineConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn validate_rejects_zero_freeze_duration() {
        let config = EngineConfig {
            freeze_duration: Duration::ZERO,
            ..EngineConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(ConversationError::InvalidConfig(_))
        ));
    }

    #[test]
    fn validate_rejects_proposal_expiration_not_above_consensus_timeout() {
        let config = EngineConfig {
            consensus_timeout: Duration::from_secs(5),
            proposal_expiration: Duration::from_secs(5),
            ..EngineConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(ConversationError::InvalidConfig(_))
        ));
    }

    #[test]
    fn validate_rejects_bad_steward_bounds() {
        let mut config = EngineConfig::default();
        config.steward_list.sn_min = 3;
        config.steward_list.sn_max = 2;
        assert!(matches!(
            config.validate(),
            Err(ConversationError::InvalidConfig(_))
        ));
    }
}
