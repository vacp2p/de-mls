//! Core library errors.

use crate::mls_crypto;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("MLS error: {0}")]
    Mls(#[from] mls_crypto::MlsError),

    #[error("Consensus error: {0}")]
    ConsensusError(#[from] hashgraph_like_consensus::error::ConsensusError),

    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),

    #[error("Message error: {0}")]
    MessageError(#[from] prost::DecodeError),

    #[error("MLS group not initialized")]
    MlsGroupNotInitialized,

    #[error("Caller is not a steward")]
    NotASteward,

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

    #[error("Non-MLS proposals found in approved queue (ids: {proposal_ids:?})")]
    UnexpectedNonMlsProposals { proposal_ids: Vec<u32> },
}
