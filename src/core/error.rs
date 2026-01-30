//! Core library errors.

use mls_crypto::error::IdentityError;
use thiserror::Error;

/// Errors that can occur in the core library.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Identity error.
    #[error("Identity error: {0}")]
    IdentityError(#[from] IdentityError),

    /// MLS group is not initialized for this handle.
    #[error("MLS group not initialized")]
    MlsGroupNotInitialized,

    /// Steward is not set for this group handle.
    #[error("Steward not set")]
    StewardNotSet,

    /// No proposals available for the requested operation.
    #[error("No proposals available")]
    NoProposals,

    /// Invalid state transition was attempted.
    #[error("Invalid state transition from {from} to {to}")]
    InvalidStateTransition { from: String, to: String },

    /// Invalid identity format.
    #[error("Invalid identity: {0}")]
    InvalidIdentity(String),

    /// MLS service error.
    #[error("MLS error: {0}")]
    MlsError(#[from] mls_crypto::MlsServiceError),

    /// Message encoding/decoding error.
    #[error("Message error: {0}")]
    MessageError(#[from] prost::DecodeError),

    /// Hex decoding error.
    #[error("Hex decode error: {0}")]
    HexError(#[from] alloy::hex::FromHexError),

    /// Consensus service error.
    #[error("Consensus error: {0}")]
    ConsensusError(#[from] hashgraph_like_consensus::error::ConsensusError),

    /// Group already exists.
    #[error("Group already exists")]
    GroupAlreadyExists,

    /// Group not found.
    #[error("Group not found")]
    GroupNotFound,

    /// Cannot send message in current state.
    #[error("Cannot send {message_type} message in {state} state")]
    InvalidStateForMessage { state: String, message_type: String },

    /// System time error.
    #[error("System time error: {0}")]
    SystemTimeError(#[from] std::time::SystemTimeError),

    /// JSON error.
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
}
