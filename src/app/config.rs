//! Group configuration for the application layer.

use std::time::Duration;

use crate::core::ProtocolConfig;

/// Default epoch duration (30 seconds).
pub const DEFAULT_EPOCH_DURATION: Duration = Duration::from_secs(30);

/// Default proposal expiration (1 hour).
pub const DEFAULT_PROPOSAL_EXPIRATION: Duration = Duration::from_secs(3600);

/// Default consensus timeout (15 seconds).
pub const DEFAULT_CONSENSUS_TIMEOUT: Duration = Duration::from_secs(15);

/// Default lifetime of a buffered membership update (in epochs).
pub const DEFAULT_PENDING_UPDATE_MAX_EPOCHS: u32 = 3;

/// Default delay before the proposal creator's auto-YES fires (10s of 15s
/// consensus timeout — leaves room for a manual NO to land first).
pub const DEFAULT_CREATOR_AUTO_VOTE_DELAY: Duration = Duration::from_secs(10);

/// Configuration for a group's epoch behavior.
///
/// Combines timing parameters (app-layer) with protocol parameters (core-layer).
#[derive(Debug, Clone)]
pub struct GroupConfig {
    /// Duration of each epoch.
    pub epoch_duration: Duration,
    /// Duration of the freeze phase before deterministic selection. Defaults to `epoch_duration / 2`.
    pub freeze_duration: Duration,
    /// How long a proposal stays active before expiring (RFC §Creating Voting Proposal).
    pub proposal_expiration: Duration,
    /// Timeout for consensus voting to complete.
    pub consensus_timeout: Duration,
    /// Maximum age (in epochs) of a buffered membership update before it is dropped.
    /// If the epoch steward fails to commit a buffered Add/Remove for this many
    /// consecutive epochs, the entry is discarded.
    pub pending_update_max_epochs: u32,
    /// Delay after proposal creation at which the creator auto-casts YES.
    /// `None` disables — the creator must vote manually from the UI.
    /// Keep below `consensus_timeout` so the auto-vote still affects the
    /// outcome. The creator can override by voting manually before the timer.
    pub creator_auto_vote_delay: Option<Duration>,
    /// Protocol configuration (steward list bounds and protocol-level flags).
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
            protocol: ProtocolConfig::default(),
        }
    }
}

impl GroupConfig {
    /// Create a new config with custom epoch duration.
    pub fn with_epoch_duration(epoch_duration: Duration) -> Self {
        Self {
            epoch_duration,
            freeze_duration: epoch_duration / 2,
            proposal_expiration: DEFAULT_PROPOSAL_EXPIRATION,
            consensus_timeout: DEFAULT_CONSENSUS_TIMEOUT,
            pending_update_max_epochs: DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
            creator_auto_vote_delay: Some(DEFAULT_CREATOR_AUTO_VOTE_DELAY),
            protocol: ProtocolConfig::default(),
        }
    }

    /// Effective freeze duration: explicit value or `epoch_duration / 2`.
    pub fn freeze_duration(&self) -> Duration {
        self.freeze_duration
    }
}
