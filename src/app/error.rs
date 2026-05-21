//! Error type for the app layer.

use std::time::SystemTimeError;

use hashgraph_like_consensus::error::ConsensusError;

use crate::{core::CoreError, ds::DeliveryServiceError, mls_crypto::MlsError};

/// Errors from User operations.
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("Conversation already exists")]
    ConversationAlreadyExists,

    #[error("Conversation not found")]
    ConversationNotFound,

    #[error("Welcome does not address this user's key package")]
    WelcomeNotForUs,

    #[error("Already leaving this conversation")]
    AlreadyLeaving,

    #[error("Cannot send message: conversation is in {0} state")]
    ConversationBlocked(String),

    #[error(
        "Lower-priority proposal blocked: an emergency criteria proposal is active (RFC partial freeze)"
    )]
    PartialFreeze,

    #[error("Transport error: {0}")]
    Transport(#[from] DeliveryServiceError),

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

    #[error("Lock poisoned: {0}")]
    LockPoisoned(&'static str),
}
