//! Per-conversation timing + protocol configuration with sensible defaults.

use std::time::Duration;

use crate::core::ProposalKind;
use crate::protos::de_mls::messages::v1::TimingConfig;

/// Wall-clock window the steward waits before batching approved proposals
/// into a commit (RFC §Inactivity Timer #1, "Commit inactivity").
pub const DEFAULT_COMMIT_INACTIVITY_DURATION: Duration = Duration::from_secs(60);

/// Lifetime of a voting proposal before it expires unvoted
/// (RFC §Creating Voting Proposal).
pub const DEFAULT_PROPOSAL_EXPIRATION: Duration = Duration::from_secs(600);

/// Library deadline for a single consensus session — bounds how long a
/// vote can stay open. MUST be `> voting_delay`.
pub const DEFAULT_CONSENSUS_TIMEOUT: Duration = Duration::from_secs(30);

/// Inactivity window during Layer 2 / Layer 3 recovery
/// (RFC §Inactivity Timer #2, "Recovery inactivity"). Typically shorter
/// than `commit_inactivity_duration` so retries don't burn a full epoch.
pub const DEFAULT_RECOVERY_INACTIVITY_DURATION: Duration = Duration::from_secs(5);

/// Per-member window to cast a manual vote before the app auto-votes
/// using `liveness_criteria_yes`. MUST be `< consensus_timeout`.
pub const DEFAULT_VOTING_DELAY: Duration = Duration::from_secs(10);

/// Auto-vote delay for steward-election proposals. Shorter than
/// `DEFAULT_VOTING_DELAY` so recovery elections converge fast.
pub const DEFAULT_ELECTION_VOTING_DELAY: Duration = Duration::from_secs(5);

pub const DEFAULT_LIVENESS_CRITERIA_YES: bool = true;

pub const DEFAULT_PENDING_UPDATE_MAX_EPOCHS: u32 = 3;

/// Default `max_reelection_attempts`. See [`crate::core::DEFAULT_MAX_RETRIES`].
pub use crate::core::DEFAULT_MAX_RETRIES;

/// Per-conversation timing config. Plug-in domains (scoring, steward list)
/// own their own configs on the respective plug-ins — see
/// [`crate::core::ScoringConfig`] and [`crate::core::StewardListConfig`].
#[derive(Debug, Clone)]
pub struct ConversationConfig {
    /// RFC §Inactivity Timer #1: how long the epoch steward has to commit
    /// approved proposals before honest members enter the freeze round.
    pub commit_inactivity_duration: Duration,
    /// Freeze window before deterministic selection. Defaults to
    /// `commit_inactivity_duration / 2`.
    pub freeze_duration: Duration,
    /// RFC §Inactivity Timer #2: shorter inactivity window applied during
    /// Layer 2 / Layer 3 recovery so retries don't burn a full epoch.
    pub recovery_inactivity_duration: Duration,
    /// How long a proposal stays active before expiring (RFC §Creating Voting Proposal).
    pub proposal_expiration: Duration,
    pub consensus_timeout: Duration,
    /// Max age (in epochs) of a buffered membership update. An entry first
    /// seen at epoch `E` is dropped once `current_epoch - E` exceeds this
    /// value (so it survives epochs `E..=E + max_age` inclusive).
    pub pending_update_max_epochs: u32,
    /// Max steward-election retries within one MLS epoch before the app
    /// surfaces "reelection stuck". `0` disables retry entirely.
    pub max_reelection_attempts: u32,
    /// Per-member window to cast a manual vote before the app auto-casts
    /// using `liveness_criteria_yes`. Relationship invariant:
    /// `voting_delay < consensus_timeout < commit_inactivity_duration`. See
    /// [`DEFAULT_VOTING_DELAY`].
    pub voting_delay: Duration,
    /// Auto-vote delay for steward-election proposals (see
    /// [`DEFAULT_ELECTION_VOTING_DELAY`]).
    pub election_voting_delay: Duration,
    /// Whether silent voters count as YES at `consensus_timeout` (RFC
    /// §Creating Voting Proposal). See [`DEFAULT_LIVENESS_CRITERIA_YES`].
    /// Also used by the auto-vote timer as the cast value.
    pub liveness_criteria_yes: bool,
}

impl Default for ConversationConfig {
    fn default() -> Self {
        Self {
            commit_inactivity_duration: DEFAULT_COMMIT_INACTIVITY_DURATION,
            freeze_duration: DEFAULT_COMMIT_INACTIVITY_DURATION / 2,
            recovery_inactivity_duration: DEFAULT_RECOVERY_INACTIVITY_DURATION,
            proposal_expiration: DEFAULT_PROPOSAL_EXPIRATION,
            consensus_timeout: DEFAULT_CONSENSUS_TIMEOUT,
            pending_update_max_epochs: DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
            max_reelection_attempts: DEFAULT_MAX_RETRIES,
            voting_delay: DEFAULT_VOTING_DELAY,
            election_voting_delay: DEFAULT_ELECTION_VOTING_DELAY,
            liveness_criteria_yes: DEFAULT_LIVENESS_CRITERIA_YES,
        }
    }
}

impl ConversationConfig {
    /// Auto-vote delay for the given proposal kind.
    pub fn voting_delay_for(&self, kind: ProposalKind) -> Duration {
        if kind.is_steward_election() {
            self.election_voting_delay
        } else {
            self.voting_delay
        }
    }

    /// Overwrite the duration fields from a wire [`TimingConfig`]. Used on
    /// the joiner side when applying `ConversationSync`. Non-timing fields
    /// (`liveness_criteria_yes`, `pending_update_max_epochs`) are not in
    /// `TimingConfig` and stay untouched.
    pub fn apply_timing(&mut self, timing: &TimingConfig) {
        // A zero wire duration would make its timer fire immediately (a
        // malformed-sync DoS); treat zero as "unset" and keep the local value.
        apply_nonzero_ms(
            &mut self.commit_inactivity_duration,
            timing.commit_inactivity_duration_ms,
        );
        apply_nonzero_ms(&mut self.freeze_duration, timing.freeze_duration_ms);
        apply_nonzero_ms(
            &mut self.recovery_inactivity_duration,
            timing.recovery_inactivity_duration_ms,
        );
        apply_nonzero_ms(&mut self.proposal_expiration, timing.proposal_expiration_ms);
        apply_nonzero_ms(&mut self.consensus_timeout, timing.consensus_timeout_ms);
    }
}

/// Overwrite `field` with `wire_ms` unless it is zero (treated as "unset").
fn apply_nonzero_ms(field: &mut Duration, wire_ms: u64) {
    if wire_ms != 0 {
        *field = Duration::from_millis(wire_ms);
    }
}

/// Build the wire [`TimingConfig`] from a [`ConversationConfig`]. Used on
/// the steward side when sending `ConversationSync` to joiners.
impl From<&ConversationConfig> for TimingConfig {
    fn from(config: &ConversationConfig) -> Self {
        Self {
            commit_inactivity_duration_ms: config.commit_inactivity_duration.as_millis() as u64,
            freeze_duration_ms: config.freeze_duration.as_millis() as u64,
            recovery_inactivity_duration_ms: config.recovery_inactivity_duration.as_millis() as u64,
            proposal_expiration_ms: config.proposal_expiration.as_millis() as u64,
            consensus_timeout_ms: config.consensus_timeout.as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TimingConfig` ↔ `ConversationConfig` round-trip preserves all
    /// five duration fields. Distinct values per field catch accidental
    /// swaps in either direction.
    #[test]
    fn timing_config_round_trip() {
        let original = ConversationConfig {
            commit_inactivity_duration: Duration::from_millis(100),
            freeze_duration: Duration::from_millis(200),
            recovery_inactivity_duration: Duration::from_millis(300),
            proposal_expiration: Duration::from_millis(400),
            consensus_timeout: Duration::from_millis(500),
            ..ConversationConfig::default()
        };
        let timing = TimingConfig::from(&original);
        let mut applied = ConversationConfig::default();
        applied.apply_timing(&timing);
        assert_eq!(
            applied.commit_inactivity_duration,
            Duration::from_millis(100)
        );
        assert_eq!(applied.freeze_duration, Duration::from_millis(200));
        assert_eq!(
            applied.recovery_inactivity_duration,
            Duration::from_millis(300)
        );
        assert_eq!(applied.proposal_expiration, Duration::from_millis(400));
        assert_eq!(applied.consensus_timeout, Duration::from_millis(500));
    }

    /// A zero wire duration is rejected as "unset"; the local value stays.
    #[test]
    fn apply_timing_ignores_zero_durations() {
        let mut config = ConversationConfig {
            consensus_timeout: Duration::from_secs(30),
            commit_inactivity_duration: Duration::from_secs(60),
            ..ConversationConfig::default()
        };
        let timing = TimingConfig {
            consensus_timeout_ms: 0,
            commit_inactivity_duration_ms: 0,
            freeze_duration_ms: 250,
            recovery_inactivity_duration_ms: 0,
            proposal_expiration_ms: 0,
        };
        config.apply_timing(&timing);
        // Zero fields keep their prior values.
        assert_eq!(config.consensus_timeout, Duration::from_secs(30));
        assert_eq!(config.commit_inactivity_duration, Duration::from_secs(60));
        // Non-zero field is applied.
        assert_eq!(config.freeze_duration, Duration::from_millis(250));
    }

    /// Steward-election proposals get the shorter `election_voting_delay`;
    /// other kinds get `voting_delay`.
    #[test]
    fn voting_delay_dispatch_on_proposal_kind() {
        let config = ConversationConfig {
            voting_delay: Duration::from_secs(7),
            election_voting_delay: Duration::from_secs(3),
            ..ConversationConfig::default()
        };
        assert_eq!(
            config.voting_delay_for(ProposalKind::Commit),
            Duration::from_secs(7)
        );
        assert_eq!(
            config.voting_delay_for(ProposalKind::StewardElection),
            Duration::from_secs(3)
        );
    }
}
