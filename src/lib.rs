use alloy::{hex::FromHexError, primitives::SignatureError, signers::local::LocalSignerError};
use ds::DeliveryServiceError;
use fred::error::RedisError;
use openmls::{error::LibraryError, prelude::*};
use openmls_rust_crypto::MemoryKeyStoreError;
use sc_key_store::KeyStoreError;
use std::{str::Utf8Error, string::FromUtf8Error};
use tokio::task::JoinError;

pub mod cli;
pub mod contact;
pub mod conversation;
pub mod identity;
pub mod user;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("Can't split the line")]
    SplitLineError,
    #[error("Failed to cancel token")]
    TokenCancellingError,

    #[error("Problem from std::io library: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Failed to send message to channel: {0}")]
    SenderError(String),

    #[error("Redis error: {0}")]
    RedisError(#[from] RedisError),
    #[error("Failed from tokio join: {0}")]
    TokioJoinError(#[from] JoinError),

    #[error("Unknown error: {0}")]
    AnyHowError(anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ContactError {
    #[error("Key package for the specified group does not exist.")]
    MissingKeyPackageForGroup,
    #[error("SmartContract address for the specified group does not exist.")]
    MissingSmartContractForGroup,
    #[error("User not found.")]
    UserNotFoundError,
    #[error("Group not found: {0}")]
    GroupNotFoundError(String),
    #[error("Duplicate user found in joiners list.")]
    DuplicateUserError,
    #[error("Group already exists")]
    GroupAlreadyExistsError,
    #[error("Invalid user address in signature.")]
    InvalidUserSignatureError,

    #[error(transparent)]
    DeliveryServiceError(#[from] DeliveryServiceError),

    #[error("Failed to parse signature: {0}")]
    AlloySignatureParsingError(#[from] SignatureError),
    #[error("JSON processing error: {0}")]
    JsonProcessingError(#[from] serde_json::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] tls_codec::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("Failed to create new key package: {0}")]
    MlsKeyPackageCreationError(#[from] KeyPackageNewError<MemoryKeyStoreError>),
    #[error(transparent)]
    MlsLibraryError(#[from] LibraryError),
    #[error("Failed to create signature: {0}")]
    MlsCryptoError(#[from] CryptoError),
    #[error("Failed to save signature key: {0}")]
    MlsKeyStoreError(#[from] MemoryKeyStoreError),
    #[error("Failed to create credential: {0}")]
    MlsCredentialError(#[from] CredentialError),
    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("User lacks connection to the smart contract.")]
    MissingSmartContractConnection,
    #[error("Group not found: {0}")]
    GroupNotFoundError(String),
    #[error("Group already exists: {0}")]
    GroupAlreadyExistsError(String),
    #[error("Unsupported message type.")]
    UnsupportedMessageType,
    #[error("User already exists: {0}")]
    UserAlreadyExistsError(String),
    #[error("Welcome message cannot be empty.")]
    EmptyWelcomeMessageError,
    #[error("Message from user is invalid")]
    InvalidChatMessageError,
    #[error("Message from server is invalid")]
    InvalidServerMessageError,
    #[error("User not found.")]
    UserNotFoundError,

    #[error(transparent)]
    DeliveryServiceError(#[from] DeliveryServiceError),
    #[error(transparent)]
    KeyStoreError(#[from] KeyStoreError),
    #[error(transparent)]
    IdentityError(#[from] IdentityError),
    #[error(transparent)]
    ContactError(#[from] ContactError),

    #[error("Error while creating MLS group: {0}")]
    MlsGroupCreationError(#[from] NewGroupError<MemoryKeyStoreError>),
    #[error("Error while adding member to MLS group: {0}")]
    MlsAddMemberError(#[from] AddMembersError<MemoryKeyStoreError>),
    #[error("Error while merging pending commit in MLS group: {0}")]
    MlsMergePendingCommitError(#[from] MergePendingCommitError<MemoryKeyStoreError>),
    #[error("Error while merging commit in MLS group: {0}")]
    MlsMergeCommitError(#[from] MergeCommitError<MemoryKeyStoreError>),
    #[error("Error processing unverified message: {0}")]
    MlsProcessMessageError(#[from] ProcessMessageError),
    #[error("Error while creating message: {0}")]
    MlsCreateMessageError(#[from] CreateMessageError),
    #[error("Failed to create staged join: {0}")]
    MlsWelcomeError(#[from] WelcomeError<MemoryKeyStoreError>),
    #[error("Failed to remove member from MLS group: {0}")]
    MlsRemoveMemberError(#[from] RemoveMembersError<MemoryKeyStoreError>),
    #[error("Failed to validate user key package: {0}")]
    MlsKeyPackageVerificationError(#[from] KeyPackageVerifyError),
    #[error("Failed to create signature key pair: {0}")]
    MlsSignatureKeyPairError(#[from] CryptoError),

    #[error("UTF-8 parsing error: {0}")]
    Utf8ParsingError(#[from] FromUtf8Error),
    #[error("UTF-8 string parsing error: {0}")]
    Utf8StringParsingError(#[from] Utf8Error),

    #[error("JSON processing error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] tls_codec::Error),

    #[error("Failed to parse address: {0}")]
    AddressParsingError(#[from] FromHexError),
    #[error("Failed to parse signer: {0}")]
    SignerParsingError(#[from] LocalSignerError),

    #[error("Signing error: {0}")]
    SigningError(#[from] alloy::signers::Error),
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Waku error: {0}")]
    WakuError(String),
    #[error("Message verification failed: {0}")]
    MessageVerificationFailed(#[from] secp256k1::Error),
    #[error("Unknown message type: {0}")]
    UnknownMessageType(String),

    #[error("Failed to decrypt message: {0}")]
    DecryptionError(String),
    #[error("Failed to encrypt message: {0}")]
    EncryptionError(String),

    #[error("An unknown error occurred: {0}")]
    UnknownError(anyhow::Error),
}
