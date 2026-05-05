//! Error type for the app layer.

use alloy::signers::local::LocalSignerError;
use hashgraph_like_consensus::error::ConsensusError;

use crate::{
    core::{CallbackError, CoreError},
    mls_crypto::MlsError,
};

/// Errors from User operations.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("Group already exists")]
    GroupAlreadyExists,

    #[error("Group not found")]
    GroupNotFound,

    #[error("MLS service not yet attached for this group (still pending join)")]
    MlsNotInitialized,

    #[error("Already leaving this group")]
    AlreadyLeaving,

    #[error("Cannot send message: group is in {0} state")]
    GroupBlocked(String),

    #[error(
        "Lower-priority proposal blocked: an emergency criteria proposal is active (RFC partial freeze)"
    )]
    PartialFreeze,

    /// Returned when a [`crate::core::GroupEventHandler`] callback fails.
    #[error("Handler callback error: {0}")]
    Callback(#[from] CallbackError),

    #[error("Core error: {0}")]
    Core(#[from] CoreError),

    #[error("Consensus error: {0}")]
    Consensus(#[from] ConsensusError),

    #[error("Message error: {0}")]
    Message(#[from] prost::DecodeError),

    #[error("System time error: {0}")]
    SystemTime(#[from] std::time::SystemTimeError),

    #[error("Signer error: {0}")]
    Signer(#[from] LocalSignerError),

    #[error("MLS error: {0}")]
    Mls(#[from] MlsError),
}

impl UserError {
    pub fn is_fatal(&self) -> bool {
        matches!(self, UserError::GroupNotFound | UserError::AlreadyLeaving)
    }
}
