//! Error type for the session layer.

use std::time::SystemTimeError;

use hashgraph_like_consensus::error::ConsensusError;

use crate::{core::CoreError, mls_crypto::MlsError};

/// Errors from operations on a single conversation session.
///
/// Registry-level failures (conversation lookup, lock poisoning, transport
/// delivery) are integrator concerns and live in the integrator's own error
/// type — see the reference `User` in the gateway crate.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
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
