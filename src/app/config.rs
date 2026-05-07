//! Per-group timing + protocol configuration with sensible defaults.

use std::time::Duration;

pub use crate::core::{
    DEFAULT_LIVENESS_CRITERIA_YES, DEFAULT_MAX_RETRIES, DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
    DEFAULT_THRESHOLD_PEER_SCORE,
};
use crate::core::{ProposalKind, StewardListConfig};

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

/// RFC §Peer Scoring `default_peer_score`: starting score for a new member.
pub const DEFAULT_PEER_SCORE: i64 = 100;

/// Fallback [`StewardListConfig`] for a group created without explicit bounds —
/// tiny groups with `sn ∈ [1, 2]`.
fn default_protocol_config() -> StewardListConfig {
    StewardListConfig::new(1, 2).expect("1..=2 is always a valid StewardListConfig range")
}

/// App-layer timing + embedded core-layer [`StewardListConfig`].
#[derive(Debug, Clone)]
pub struct GroupConfig {
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
    /// Max age (in epochs) of a buffered membership update. If the epoch
    /// steward fails to commit a buffered Add/Remove for this many
    /// consecutive epochs, the entry is dropped.
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
    /// RFC §Peer Scoring `default_peer_score`. See [`DEFAULT_PEER_SCORE`].
    pub default_peer_score: i64,
    /// RFC §Peer Scoring `threshold_peer_score`. See [`DEFAULT_THRESHOLD_PEER_SCORE`].
    pub threshold_peer_score: i64,
    pub protocol: StewardListConfig,
}

impl Default for GroupConfig {
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
            default_peer_score: DEFAULT_PEER_SCORE,
            threshold_peer_score: DEFAULT_THRESHOLD_PEER_SCORE,
            protocol: default_protocol_config(),
        }
    }
}

impl GroupConfig {
    /// Default config with a custom commit-inactivity window; freeze becomes
    /// `commit_inactivity_duration / 2`.
    pub fn with_commit_inactivity_duration(commit_inactivity_duration: Duration) -> Self {
        Self {
            commit_inactivity_duration,
            freeze_duration: commit_inactivity_duration / 2,
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
