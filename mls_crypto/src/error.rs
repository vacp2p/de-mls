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

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
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
    #[error("Group still active")]
    GroupStillActive,
}
