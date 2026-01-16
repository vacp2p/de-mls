use alloy::signers::local::LocalSignerError;
use std::env::VarError;
use std::num::ParseIntError;
use std::string::FromUtf8Error;

use ds::DeliveryServiceError;
use mls_crypto::error::{IdentityError, MlsServiceError};

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("Failed to verify signature: {0}")]
    InvalidSignature(#[from] libsecp256k1::Error),
    #[error("JSON processing error: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("Invalid alloy signature: {0}")]
    InvalidAlloySignature(#[from] alloy::primitives::SignatureError),
    #[error("Mismatched length: expected {expect}, got {actual}")]
    MismatchedLength { expect: usize, actual: usize },
}

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error(transparent)]
    MessageError(#[from] MessageError),
    #[error(transparent)]
    IdentityError(#[from] IdentityError),

    #[error("Steward not set")]
    StewardNotSet,
    #[error("MLS group not initialized")]
    MlsGroupNotSet,
    #[error(transparent)]
    MlsServiceError(#[from] MlsServiceError),
    #[error("Invalid state transition from {from} to {to}")]
    InvalidStateTransition { from: String, to: String },
    #[error("Empty proposals for current epoch")]
    EmptyProposals,
    #[error("Invalid state [{state}] to send message [{message_type}]")]
    InvalidStateToMessageSend { state: String, message_type: String },

    #[error("Failed to decode hex address: {0}")]
    HexDecodeError(#[from] alloy::hex::FromHexError),
    #[error("Failed to decode app message: {0}")]
    AppMessageDecodeError(#[from] prost::DecodeError),
}

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error(transparent)]
    DeliveryServiceError(#[from] DeliveryServiceError),
    #[error(transparent)]
    IdentityError(#[from] IdentityError),
    #[error(transparent)]
    GroupError(#[from] GroupError),
    #[error(transparent)]
    MessageError(#[from] MessageError),
    #[error(transparent)]
    MlsServiceError(#[from] MlsServiceError),
    #[error(transparent)]
    ConsensusServiceError(#[from] hashgraph_like_consensus::error::ConsensusError),

    #[error("Group already exists")]
    GroupAlreadyExistsError,
    #[error("Group not found")]
    GroupNotFoundError,
    #[error("MLS group not initialized")]
    MlsGroupNotInitialized,
    #[error("Welcome message cannot be empty.")]
    EmptyWelcomeMessageError,
    #[error("Message verification failed")]
    MessageVerificationFailed,
    #[error("Invalid user action: {0}")]
    InvalidUserAction(String),
    #[error("Unknown content topic type: {0}")]
    UnknownContentTopicType(String),
    #[error("Invalid group state: {0}")]
    InvalidGroupState(String),
    #[error("No proposals found")]
    NoProposalsFound,
    #[error("Invalid app message type")]
    InvalidAppMessageType,

    #[error("UTF-8 parsing error: {0}")]
    Utf8ParsingError(#[from] FromUtf8Error),
    #[error("Failed to parse signer: {0}")]
    SignerParsingError(#[from] LocalSignerError),
    #[error("Failed to decode welcome message: {0}")]
    WelcomeMessageDecodeError(#[from] prost::DecodeError),
    #[error("Failed to get current time")]
    FailedToGetCurrentTime(#[from] std::time::SystemTimeError),
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Failed to read env var {0}: {1}")]
    EnvVar(&'static str, #[source] VarError),

    #[error("Failed to parse int: {0}")]
    ParseInt(#[from] ParseIntError),

    #[error(transparent)]
    DeliveryServiceError(#[from] DeliveryServiceError),
}
