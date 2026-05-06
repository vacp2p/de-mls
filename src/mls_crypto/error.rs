//! Error type for MLS operations.
//!
//! Variants that wrap an OpenMLS error type generic over the storage
//! backend ([`ProcessMessageError`], [`WelcomeError`], …) erase that
//! parameter via `Box<dyn std::error::Error + Send + Sync>` so a single
//! [`MlsError`] can flow regardless of which storage backend the
//! [`OpenMlsService`](crate::mls_crypto::OpenMlsService) is configured
//! with. The blanket `From` impls below box automatically, so `?` keeps
//! working at call sites.

use std::error::Error as StdError;

use openmls::{
    error::LibraryError,
    group::{NewGroupError, ProposeRemoveMemberError, WelcomeError},
    prelude::{
        CommitToPendingProposalsError, CreateMessageError, KeyPackageNewError, MergeCommitError,
        MergePendingCommitError, ProcessMessageError, ProposeAddMemberError,
    },
};
use openmls_traits::types::CryptoError;

/// Boxed `std::error::Error` used as the storage-error payload in any
/// `MlsError` variant whose source type is generic over the storage
/// backend.
pub type BoxedError = Box<dyn StdError + Send + Sync>;

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

    // ── MLS wire format (not parameterised over storage) ──
    #[error(transparent)]
    MlsMessageDeserialize(#[from] openmls::prelude::Error),

    #[error(transparent)]
    ProtocolMessage(#[from] openmls::framing::errors::ProtocolMessageError),

    #[error(transparent)]
    MlsMessageSerialize(#[from] openmls::framing::errors::MlsMessageError),

    #[error("Invalid key package bytes: {0}")]
    KeyPackageJson(serde_json::Error),

    // ── MLS group operations (storage-error payloads erased) ──
    #[error(transparent)]
    ProcessMessage(BoxedError),

    #[error(transparent)]
    CreateMessage(#[from] CreateMessageError),

    #[error(transparent)]
    MergeCommit(BoxedError),

    #[error(transparent)]
    MergePendingCommit(BoxedError),

    #[error(transparent)]
    CommitToPendingProposals(BoxedError),

    #[error(transparent)]
    ProposeAddMember(BoxedError),

    #[error(transparent)]
    ProposeRemoveMember(BoxedError),

    #[error(transparent)]
    NewGroup(BoxedError),

    #[error(transparent)]
    Welcome(BoxedError),

    // ── Storage backend (erased) ──
    #[error(transparent)]
    MlsStorage(BoxedError),

    #[error("Lock poisoned: {0}")]
    Lock(String),

    // ── Semantic ──
    #[error("Unexpected MLS message type")]
    UnexpectedMessageType,

    #[error("Group not found: {0}")]
    GroupNotFound(String),

    #[error("No pending staged commit for group: {0}")]
    NoPendingStagedCommit(String),

    #[error("Remove proposal references leaf index {0} with no active credential")]
    UnknownLeafIndex(u32),
}

impl MlsError {
    /// Box any storage-backend error into [`MlsError::MlsStorage`]. Used in
    /// `.map_err(MlsError::storage)?` at call sites whose error type is the
    /// storage's concrete `Error` (which can't be auto-converted via
    /// blanket `From<E>` without colliding with existing impls).
    pub fn storage<E: StdError + Send + Sync + 'static>(e: E) -> Self {
        MlsError::MlsStorage(Box::new(e))
    }
}

// ── Boxing From impls for storage-parameterised OpenMLS error types ──
//
// Each impl is generic over `E: StdError + Send + Sync + 'static` so any
// storage backend's error type works.

impl<E: StdError + Send + Sync + 'static> From<ProcessMessageError<E>> for MlsError {
    fn from(e: ProcessMessageError<E>) -> Self {
        MlsError::ProcessMessage(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<MergeCommitError<E>> for MlsError {
    fn from(e: MergeCommitError<E>) -> Self {
        MlsError::MergeCommit(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<MergePendingCommitError<E>> for MlsError {
    fn from(e: MergePendingCommitError<E>) -> Self {
        MlsError::MergePendingCommit(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<CommitToPendingProposalsError<E>> for MlsError {
    fn from(e: CommitToPendingProposalsError<E>) -> Self {
        MlsError::CommitToPendingProposals(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<ProposeAddMemberError<E>> for MlsError {
    fn from(e: ProposeAddMemberError<E>) -> Self {
        MlsError::ProposeAddMember(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<ProposeRemoveMemberError<E>> for MlsError {
    fn from(e: ProposeRemoveMemberError<E>) -> Self {
        MlsError::ProposeRemoveMember(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<NewGroupError<E>> for MlsError {
    fn from(e: NewGroupError<E>) -> Self {
        MlsError::NewGroup(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<WelcomeError<E>> for MlsError {
    fn from(e: WelcomeError<E>) -> Self {
        MlsError::Welcome(Box::new(e))
    }
}

/// Recover a poisoned lock as [`MlsError::Lock`].
impl<T> From<std::sync::PoisonError<T>> for MlsError {
    fn from(e: std::sync::PoisonError<T>) -> Self {
        MlsError::Lock(e.to_string())
    }
}
