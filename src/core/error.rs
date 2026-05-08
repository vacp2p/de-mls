//! Core library errors.

use crate::mls_crypto;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// MLS error (covers identity, service, and storage variants).
    #[error("MLS error: {0}")]
    Mls(#[from] mls_crypto::MlsError),

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

    /// MLS group is not initialized for this conversation.
    #[error("MLS group not initialized")]
    MlsGroupNotInitialized,

    /// Caller is not a steward of this conversation.
    #[error("caller is not a steward")]
    NotASteward,

    /// No proposals available for the requested operation.
    #[error("No proposals available")]
    NoProposals,

    #[error("Invalid conversation update request")]
    InvalidConversationUpdateRequest,

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
