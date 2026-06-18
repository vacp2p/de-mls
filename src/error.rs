//! Library error types: the protocol-level [`CoreError`] and the
//! per-conversation [`ConversationError`] raised by conversation operations.

use std::time::SystemTimeError;

use hashgraph_like_consensus::error::ConsensusError;

use crate::mls_crypto::{self, MlsError};

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

    #[error("Empty members list")]
    EmptyMembersList,

    #[error("Invalid config size")]
    InvalidConfigSize,

    #[error("Non-MLS proposals found in approved queue (ids: {proposal_ids:?})")]
    UnexpectedNonMlsProposals { proposal_ids: Vec<u32> },
}

/// Errors from operations on a single conversation.
///
/// Registry-level failures (conversation lookup, lock poisoning, transport
/// delivery) are integrator concerns and live in the integrator's own error
/// type — see the reference `User` in the gateway crate.
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("Cannot send message: conversation is in {0} state")]
    ConversationBlocked(String),

    #[error(
        "Lower-priority proposal blocked: an emergency criteria proposal is active (RFC partial freeze)"
    )]
    PartialFreeze,

    #[error("Core error: {0}")]
    Core(#[from] CoreError),

    #[error("Consensus error: {0}")]
    Consensus(#[from] ConsensusError),

    #[error("Message error: {0}")]
    Message(#[from] prost::DecodeError),

    #[error("System time error: {0}")]
    SystemTime(#[from] SystemTimeError),

    #[error("MLS error: {0}")]
    Mls(#[from] MlsError),
}
