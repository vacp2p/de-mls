//! Group configuration for the application layer.

use std::time::Duration;

use crate::core::ProtocolConfig;

/// Default epoch duration (30 seconds).
pub const DEFAULT_EPOCH_DURATION: Duration = Duration::from_secs(30);

/// Default proposal expiration (1 hour).
pub const DEFAULT_PROPOSAL_EXPIRATION: Duration = Duration::from_secs(3600);

/// Default consensus timeout (15 seconds).
pub const DEFAULT_CONSENSUS_TIMEOUT: Duration = Duration::from_secs(15);

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
            protocol: ProtocolConfig::default(),
        }
    }

    /// Effective freeze duration: explicit value or `epoch_duration / 2`.
    pub fn freeze_duration(&self) -> Duration {
        self.freeze_duration
    }
}
