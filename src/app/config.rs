//! Per-group timing + protocol configuration with sensible defaults.

use std::time::Duration;

use crate::core::ProtocolConfig;

pub const DEFAULT_EPOCH_DURATION: Duration = Duration::from_secs(30);
pub const DEFAULT_PROPOSAL_EXPIRATION: Duration = Duration::from_secs(3600);
pub const DEFAULT_CONSENSUS_TIMEOUT: Duration = Duration::from_secs(15);
pub const DEFAULT_PENDING_UPDATE_MAX_EPOCHS: u32 = 3;

/// Fires 10s into the 15s consensus window — leaves room for a manual NO
/// to land first.
pub const DEFAULT_CREATOR_AUTO_VOTE_DELAY: Duration = Duration::from_secs(10);

/// One retry gives the responsible proposer a second shot with a different
/// list composition; beyond that human/policy intervention is expected.
pub const DEFAULT_MAX_REELECTION_RETRIES: u32 = 1;

/// Fallback [`ProtocolConfig`] for a group created without explicit bounds —
/// tiny groups with `sn ∈ [1, 2]`. Real deployments override.
fn default_protocol_config() -> ProtocolConfig {
    ProtocolConfig::new(1, 2).expect("1..=2 is always a valid ProtocolConfig range")
}

/// App-layer timing + embedded core-layer [`ProtocolConfig`].
#[derive(Debug, Clone)]
pub struct GroupConfig {
    pub epoch_duration: Duration,
    /// Freeze window before deterministic selection. Defaults to `epoch_duration / 2`.
    pub freeze_duration: Duration,
    /// How long a proposal stays active before expiring (RFC §Creating Voting Proposal).
    pub proposal_expiration: Duration,
    pub consensus_timeout: Duration,
    /// Max age (in epochs) of a buffered membership update. If the epoch
    /// steward fails to commit a buffered Add/Remove for this many
    /// consecutive epochs, the entry is dropped.
    pub pending_update_max_epochs: u32,
    /// Delay after proposal creation at which the creator auto-casts YES.
    /// `None` disables — the creator must vote manually. Keep below
    /// `consensus_timeout` so the auto-vote still affects the outcome.
    pub creator_auto_vote_delay: Option<Duration>,
    /// Max steward-election retries within one MLS epoch before the app
    /// surfaces "reelection stuck". `0` disables retry entirely.
    pub max_reelection_retries: u32,
    pub protocol: ProtocolConfig,
}

impl Default for GroupConfig {
    fn default() -> Self {
        Self {
            epoch_duration: DEFAULT_EPOCH_DURATION,
            freeze_duration: DEFAULT_EPOCH_DURATION / 2,
            proposal_expiration: DEFAULT_PROPOSAL_EXPIRATION,
            consensus_timeout: DEFAULT_CONSENSUS_TIMEOUT,
            pending_update_max_epochs: DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
            creator_auto_vote_delay: Some(DEFAULT_CREATOR_AUTO_VOTE_DELAY),
            max_reelection_retries: DEFAULT_MAX_REELECTION_RETRIES,
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
}
