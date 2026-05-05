//! Error type for MLS operations.

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
pub enum MlsError {
    // ── Identity ──
    #[error(transparent)]
    UnableToCreateKeyPackage(#[from] KeyPackageNewError),

    #[error("Invalid hash reference: {0}")]
    InvalidHashRef(#[from] LibraryError),

    #[error("Failed to create signer: {0}")]
    UnableToCreateSigner(#[from] CryptoError),

    #[error("JSON encoding error: {0}")]
    InvalidJson(serde_json::Error),

    #[error("Invalid wallet address: {0}")]
    InvalidWalletAddress(String),

    // ── MLS wire format ──
    #[error(transparent)]
    MlsMessageDeserialize(#[from] openmls::prelude::Error),

    #[error(transparent)]
    ProtocolMessage(#[from] openmls::framing::errors::ProtocolMessageError),

    #[error(transparent)]
    MlsMessageSerialize(#[from] openmls::framing::errors::MlsMessageError),

    #[error("Invalid key package bytes: {0}")]
    KeyPackageJson(serde_json::Error),

    // ── MLS group operations ──
    #[error(transparent)]
    ProcessMessage(#[from] ProcessMessageError<MemoryStorageError>),

    #[error(transparent)]
    CreateMessage(#[from] CreateMessageError),

    #[error(transparent)]
    MergeCommit(#[from] MergeCommitError<MemoryStorageError>),

    #[error(transparent)]
    MergePendingCommit(#[from] MergePendingCommitError<MemoryStorageError>),

    #[error(transparent)]
    CommitToPendingProposals(#[from] CommitToPendingProposalsError<MemoryStorageError>),

    #[error(transparent)]
    ProposeAddMember(#[from] ProposeAddMemberError<MemoryStorageError>),

    #[error(transparent)]
    ProposeRemoveMember(#[from] ProposeRemoveMemberError<MemoryStorageError>),

    #[error(transparent)]
    NewGroup(#[from] NewGroupError<MemoryStorageError>),

    #[error(transparent)]
    Welcome(#[from] WelcomeError<MemoryStorageError>),

    // ── Storage / locking ──
    #[error(transparent)]
    MlsStorage(#[from] MemoryStorageError),

    #[error("Lock poisoned: {0}")]
    Lock(String),

    // ── Semantic (no inner error to inherit Display from) ──
    #[error("Unexpected MLS message type")]
    UnexpectedMessageType,

    #[error("Group not found: {0}")]
    GroupNotFound(String),

    #[error("Welcome message not for this user")]
    WelcomeNotForUs,

    #[error("No pending staged commit for group: {0}")]
    NoPendingStagedCommit(String),

    #[error("Remove proposal references leaf index {0} with no active credential")]
    UnknownLeafIndex(u32),
}

/// Recover a poisoned lock as [`MlsError::Lock`].
impl<T> From<std::sync::PoisonError<T>> for MlsError {
    fn from(e: std::sync::PoisonError<T>) -> Self {
        MlsError::Lock(e.to_string())
    }
}
