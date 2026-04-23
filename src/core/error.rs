//! Core library errors.

use crate::mls_crypto;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Identity error.
    #[error("Identity error: {0}")]
    IdentityError(#[from] mls_crypto::IdentityError),

    /// MLS service error.
    #[error("MLS error: {0}")]
    MlsServiceError(#[from] mls_crypto::MlsServiceError),

    /// Storage error.
    #[error("Storage error: {0}")]
    StorageError(#[from] mls_crypto::StorageError),

    #[error("Consensus error: {0}")]
    ConsensusError(#[from] hashgraph_like_consensus::error::ConsensusError),

    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),

    /// Message encoding/decoding error.
    #[error("Message error: {0}")]
    MessageError(#[from] prost::DecodeError),

    /// JSON error.
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    /// MLS group is not initialized for this group.
    #[error("MLS group not initialized")]
    MlsGroupNotInitialized,

    /// Caller is not a steward of this group.
    #[error("caller is not a steward")]
    NotASteward,

    /// No proposals available for the requested operation.
    #[error("No proposals available")]
    NoProposals,

    #[error("Invalid group update request")]
    InvalidGroupUpdateRequest,

    #[error("Invalid subtopic: {0}")]
    InvalidSubtopic(String),

    #[error("Empty members list")]
    EmptyMembersList,

    #[error("Invalid config size")]
    InvalidConfigSize,

    /// Governance proposals (emergency criteria, steward election) slipped into
    /// the approved queue — `apply_consensus_result` should have removed them
    /// before `create_commit_candidate` ran.
    #[error(
        "Non-MLS proposals found in approved queue (ids: {proposal_ids:?}). \
         They should have been removed by apply_consensus_result."
    )]
    UnexpectedNonMlsProposals { proposal_ids: Vec<u32> },
}

impl From<mls_crypto::MlsError> for CoreError {
    fn from(e: mls_crypto::MlsError) -> Self {
        match e {
            mls_crypto::MlsError::Identity(e) => CoreError::IdentityError(e),
            mls_crypto::MlsError::Service(e) => CoreError::MlsServiceError(e),
            mls_crypto::MlsError::Storage(e) => CoreError::StorageError(e),
        }
    }
}
