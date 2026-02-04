//! Core library errors.

use mls_crypto::error::{IdentityError, MlsServiceError};
use thiserror::Error;

/// Errors that can occur in the core library.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Identity error.
    #[error("Identity error: {0}")]
    IdentityError(#[from] IdentityError),

    /// MLS service error.
    #[error("MLS error: {0}")]
    MlsError(#[from] MlsServiceError),

    /// Message encoding/decoding error.
    #[error("Message error: {0}")]
    MessageError(#[from] prost::DecodeError),

    /// JSON error.
    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    /// MLS group is not initialized for this handle.
    #[error("MLS group not initialized")]
    MlsGroupNotInitialized,

    /// Steward is not set for this group handle.
    #[error("Steward not set")]
    StewardNotSet,

    /// No proposals available for the requested operation.
    #[error("No proposals available")]
    NoProposals,

    #[error("Invalid group update request")]
    InvalidGroupUpdateRequest,

    /// Generic handler failure.
    #[error("Handler error: {0}")]
    HandlerError(String),

    /// Transport/delivery failure.
    #[error("Delivery error: {0}")]
    DeliveryError(String),

    #[error("Invalid subtopic: {0}")]
    InvalidSubtopic(String),
}
