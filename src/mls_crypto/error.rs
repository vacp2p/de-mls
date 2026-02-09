//! Error types for MLS operations.

use openmls::{
    error::LibraryError,
    group::{NewGroupError, ProposeRemoveMemberError, WelcomeError},
    prelude::{
        CommitToPendingProposalsError, CreateMessageError, KeyPackageNewError, MergeCommitError,
        MergePendingCommitError, ProcessMessageError, ProposeAddMemberError,
    },
};
use openmls_rust_crypto::MemoryStorageError;
use openmls_traits::types::CryptoError;

/// Result type alias for MLS operations.
pub type Result<T> = std::result::Result<T, MlsError>;

/// Storage operation errors.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("Storage I/O error: {0}")]
    Io(String),

    #[error("Storage serialization error: {0}")]
    Serialization(String),

    #[error("Storage lock error: {0}")]
    Lock(String),

    #[error("Storage backend error: {0}")]
    Backend(String),
}

/// Identity-specific errors.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("Identity not initialized")]
    IdentityNotFound,

    #[error("Identity already initialized")]
    AlreadyInitialized,

    #[error(transparent)]
    UnableToCreateKeyPackage(#[from] KeyPackageNewError),

    #[error("Invalid hash reference: {0}")]
    InvalidHashRef(#[from] LibraryError),

    #[error("Unable to create new signer: {0}")]
    UnableToCreateSigner(#[from] CryptoError),

    #[error("Unable to save signature key: {0}")]
    UnableToSaveSignatureKey(#[from] MemoryStorageError),

    #[error("Invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),

    #[error("Invalid wallet address: {0}")]
    InvalidWalletAddress(String),
}

/// MLS service operation errors.
#[derive(Debug, thiserror::Error)]
pub enum MlsServiceError {
    #[error("Failed to deserialize MLS message: {0}")]
    MlsMessageInDeserialize(#[from] openmls::prelude::Error),

    #[error("Failed to convert to protocol message: {0}")]
    ProtocolMessage(#[from] openmls::framing::errors::ProtocolMessageError),

    #[error("Failed to process MLS message: {0}")]
    ProcessMessage(#[from] ProcessMessageError<MemoryStorageError>),

    #[error("Failed to create MLS message: {0}")]
    CreateMessage(#[from] CreateMessageError),

    #[error("Failed to merge staged commit: {0}")]
    MergeCommit(#[from] MergeCommitError<MemoryStorageError>),

    #[error("Failed to merge pending commit: {0}")]
    MergePendingCommit(#[from] MergePendingCommitError<MemoryStorageError>),

    #[error("Failed to commit to pending proposals: {0}")]
    CommitToPendingProposals(#[from] CommitToPendingProposalsError<MemoryStorageError>),

    #[error("Failed to add member proposal: {0}")]
    ProposeAddMember(#[from] ProposeAddMemberError<MemoryStorageError>),

    #[error("Failed to remove member proposal: {0}")]
    ProposeRemoveMember(#[from] ProposeRemoveMemberError<MemoryStorageError>),

    #[error("Failed to create MLS group: {0}")]
    NewGroup(#[from] NewGroupError<MemoryStorageError>),

    #[error("Failed to join MLS group: {0}")]
    Welcome(#[from] WelcomeError<MemoryStorageError>),

    #[error("Failed to store pending proposal: {0}")]
    StorePendingProposal(#[from] MemoryStorageError),

    #[error("Failed to serialize MLS message: {0}")]
    MlsMessage(#[from] openmls::framing::errors::MlsMessageError),

    #[error("Invalid key package bytes: {0}")]
    InvalidKeyPackage(#[from] serde_json::Error),

    #[error("Unexpected MLS message type")]
    UnexpectedMessageType,

    #[error("Group not found: {0}")]
    GroupNotFound(String),

    #[error("Group still active")]
    GroupStillActive,

    #[error("Welcome message not for this user")]
    WelcomeNotForUs,
}

/// Unified MLS error type.
#[derive(Debug, thiserror::Error)]
pub enum MlsError {
    #[error(transparent)]
    Identity(#[from] IdentityError),

    #[error(transparent)]
    Service(#[from] MlsServiceError),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

// Convenience From impls for MlsError
impl From<KeyPackageNewError> for MlsError {
    fn from(e: KeyPackageNewError) -> Self {
        MlsError::Identity(IdentityError::UnableToCreateKeyPackage(e))
    }
}

impl From<CryptoError> for MlsError {
    fn from(e: CryptoError) -> Self {
        MlsError::Identity(IdentityError::UnableToCreateSigner(e))
    }
}

impl From<MemoryStorageError> for MlsError {
    fn from(e: MemoryStorageError) -> Self {
        MlsError::Service(MlsServiceError::StorePendingProposal(e))
    }
}

impl From<NewGroupError<MemoryStorageError>> for MlsError {
    fn from(e: NewGroupError<MemoryStorageError>) -> Self {
        MlsError::Service(MlsServiceError::NewGroup(e))
    }
}

impl From<WelcomeError<MemoryStorageError>> for MlsError {
    fn from(e: WelcomeError<MemoryStorageError>) -> Self {
        MlsError::Service(MlsServiceError::Welcome(e))
    }
}

impl From<ProcessMessageError<MemoryStorageError>> for MlsError {
    fn from(e: ProcessMessageError<MemoryStorageError>) -> Self {
        MlsError::Service(MlsServiceError::ProcessMessage(e))
    }
}

impl From<CreateMessageError> for MlsError {
    fn from(e: CreateMessageError) -> Self {
        MlsError::Service(MlsServiceError::CreateMessage(e))
    }
}

impl From<MergeCommitError<MemoryStorageError>> for MlsError {
    fn from(e: MergeCommitError<MemoryStorageError>) -> Self {
        MlsError::Service(MlsServiceError::MergeCommit(e))
    }
}

impl From<ProposeAddMemberError<MemoryStorageError>> for MlsError {
    fn from(e: ProposeAddMemberError<MemoryStorageError>) -> Self {
        MlsError::Service(MlsServiceError::ProposeAddMember(e))
    }
}

impl From<ProposeRemoveMemberError<MemoryStorageError>> for MlsError {
    fn from(e: ProposeRemoveMemberError<MemoryStorageError>) -> Self {
        MlsError::Service(MlsServiceError::ProposeRemoveMember(e))
    }
}

impl From<CommitToPendingProposalsError<MemoryStorageError>> for MlsError {
    fn from(e: CommitToPendingProposalsError<MemoryStorageError>) -> Self {
        MlsError::Service(MlsServiceError::CommitToPendingProposals(e))
    }
}

impl From<MergePendingCommitError<MemoryStorageError>> for MlsError {
    fn from(e: MergePendingCommitError<MemoryStorageError>) -> Self {
        MlsError::Service(MlsServiceError::MergePendingCommit(e))
    }
}

impl From<openmls::prelude::Error> for MlsError {
    fn from(e: openmls::prelude::Error) -> Self {
        MlsError::Service(MlsServiceError::MlsMessageInDeserialize(e))
    }
}

impl From<openmls::framing::errors::ProtocolMessageError> for MlsError {
    fn from(e: openmls::framing::errors::ProtocolMessageError) -> Self {
        MlsError::Service(MlsServiceError::ProtocolMessage(e))
    }
}

impl From<openmls::framing::errors::MlsMessageError> for MlsError {
    fn from(e: openmls::framing::errors::MlsMessageError) -> Self {
        MlsError::Service(MlsServiceError::MlsMessage(e))
    }
}

impl From<openmls::error::LibraryError> for MlsError {
    fn from(e: openmls::error::LibraryError) -> Self {
        MlsError::Identity(IdentityError::InvalidHashRef(e))
    }
}
