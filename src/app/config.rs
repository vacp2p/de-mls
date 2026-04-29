//! Per-group timing + protocol configuration with sensible defaults.

use std::time::Duration;

pub use crate::core::DEFAULT_MAX_REELECTION_ATTEMPTS;
use crate::core::{ProposalKind, ProtocolConfig};

/// Wall-clock window the steward waits before batching approved proposals
/// into a commit.
pub const DEFAULT_EPOCH_DURATION: Duration = Duration::from_secs(60);

/// Lifetime of a voting proposal before it expires unvoted
/// (RFC §Creating Voting Proposal).
pub const DEFAULT_PROPOSAL_EXPIRATION: Duration = Duration::from_secs(3600);

/// Library deadline for a single consensus session — bounds how long a
/// vote can stay open. MUST be `> voting_delay`.
pub const DEFAULT_CONSENSUS_TIMEOUT: Duration = Duration::from_secs(30);

/// Drop a buffered membership update if the steward fails to commit it
/// for this many consecutive epochs.
pub const DEFAULT_PENDING_UPDATE_MAX_EPOCHS: u32 = 3;

/// Inactivity window during recovery. Reset to `epoch_duration` after a
/// successful commit.
pub const DEFAULT_RETRY_INACTIVITY_DURATION: Duration = Duration::from_secs(5);

/// Per-member window to cast a manual vote before the app auto-votes
/// using `liveness_criteria_yes`. MUST be `< consensus_timeout`.
pub const DEFAULT_VOTING_DELAY: Duration = Duration::from_secs(10);

/// Auto-vote delay for steward-election proposals. Shorter than
/// `DEFAULT_VOTING_DELAY` so recovery elections converge fast.
pub const DEFAULT_ELECTION_VOTING_DELAY: Duration = Duration::from_secs(5);

/// Whether silent voters count as YES at `consensus_timeout`
/// (RFC §Creating Voting Proposal). Also used as the auto-vote value.
pub const DEFAULT_LIVENESS_CRITERIA_YES: bool = true;

/// RFC §Peer Scoring `default_peer_score`: starting score for a new member.
pub const DEFAULT_PEER_SCORE: i64 = 100;

/// RFC §Peer Scoring `threshold_peer_score`: at or below this, a member
/// becomes eligible for `SCORE_BELOW_THRESHOLD` ECP removal.
pub const DEFAULT_THRESHOLD_PEER_SCORE: i64 = 0;

/// Fallback [`ProtocolConfig`] for a group created without explicit bounds —
/// tiny groups with `sn ∈ [1, 2]`.
fn default_protocol_config() -> ProtocolConfig {
    ProtocolConfig::new(1, 2).expect("1..=2 is always a valid ProtocolConfig range")
}

/// App-layer timing + embedded core-layer [`ProtocolConfig`].
#[derive(Debug, Clone)]
pub struct GroupConfig {
    pub epoch_duration: Duration,
    /// Freeze window before deterministic selection. Defaults to `epoch_duration / 2`.
    pub freeze_duration: Duration,
    /// Inactivity window during recovery. Much shorter than
    /// `epoch_duration` so retries don't burn another full epoch.
    pub retry_inactivity_duration: Duration,
    /// How long a proposal stays active before expiring (RFC §Creating Voting Proposal).
    pub proposal_expiration: Duration,
    pub consensus_timeout: Duration,
    /// Max age (in epochs) of a buffered membership update. If the epoch
    /// steward fails to commit a buffered Add/Remove for this many
    /// consecutive epochs, the entry is dropped.
    pub pending_update_max_epochs: u32,
    /// Max steward-election retries within one MLS epoch before the app
    /// surfaces "reelection stuck". `0` disables retry entirely.
    pub max_reelection_attempts: u32,
    /// Per-member window to cast a manual vote before the app auto-casts
    /// using `liveness_criteria_yes`. Relationship invariant:
    /// `voting_delay < consensus_timeout < epoch_duration`. See
    /// [`DEFAULT_VOTING_DELAY`].
    pub voting_delay: Duration,
    /// Auto-vote delay for steward-election proposals (see
    /// [`DEFAULT_ELECTION_VOTING_DELAY`]).
    pub election_voting_delay: Duration,
    /// Whether silent voters count as YES at `consensus_timeout` (RFC
    /// §Creating Voting Proposal). See [`DEFAULT_LIVENESS_CRITERIA_YES`].
    /// Also used by the auto-vote timer as the cast value.
    pub liveness_criteria_yes: bool,
    /// RFC §Peer Scoring `default_peer_score`. See [`DEFAULT_PEER_SCORE`].
    pub default_peer_score: i64,
    /// RFC §Peer Scoring `threshold_peer_score`. See [`DEFAULT_THRESHOLD_PEER_SCORE`].
    pub threshold_peer_score: i64,
    pub protocol: ProtocolConfig,
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            epoch_duration: DEFAULT_EPOCH_DURATION,
            freeze_duration: DEFAULT_EPOCH_DURATION / 2,
            retry_inactivity_duration: DEFAULT_RETRY_INACTIVITY_DURATION,
            proposal_expiration: DEFAULT_PROPOSAL_EXPIRATION,
            consensus_timeout: DEFAULT_CONSENSUS_TIMEOUT,
            pending_update_max_epochs: DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
            max_reelection_attempts: DEFAULT_MAX_REELECTION_ATTEMPTS,
            voting_delay: DEFAULT_VOTING_DELAY,
            election_voting_delay: DEFAULT_ELECTION_VOTING_DELAY,
            liveness_criteria_yes: DEFAULT_LIVENESS_CRITERIA_YES,
            default_peer_score: DEFAULT_PEER_SCORE,
            threshold_peer_score: DEFAULT_THRESHOLD_PEER_SCORE,
            protocol: default_protocol_config(),
        }
    }
}

impl GroupConfig {
    /// Default config with a custom epoch; freeze becomes `epoch_duration / 2`.
    pub fn with_epoch_duration(epoch_duration: Duration) -> Self {
        Self {
            epoch_duration,
            freeze_duration: epoch_duration / 2,
            ..Self::default()
        }
    }

    /// Auto-vote delay for the given proposal kind.
    pub fn voting_delay_for(&self, kind: ProposalKind) -> Duration {
        if kind.is_steward_election() {
            self.election_voting_delay
        } else {
            self.voting_delay
        }
    }
}
