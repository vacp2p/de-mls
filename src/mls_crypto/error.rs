//! Error type for MLS operations.
//!
//! Variants that wrap an OpenMLS error type generic over the storage
//! backend ([`ProcessMessageError`], [`WelcomeError`], …) so a single
//! [`MlsError`] can flow regardless of which storage backend the
//! [`MlsService`](crate::mls_crypto::MlsService) runs over.

use std::error::Error as StdError;

use openmls::prelude::tls_codec::Error as TlsCodecError;
use openmls::{
    group::{CommitBuilderStageError, CreateCommitError, NewGroupError, WelcomeError},
    prelude::{CreateMessageError, MergeCommitError, MergePendingCommitError, ProcessMessageError},
};

/// Boxed `std::error::Error` used as the storage-error payload in any
/// `MlsError` variant whose source type is generic over the storage
/// backend.
pub type BoxedError = Box<dyn StdError + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum MlsError {
    // ── Crypto + key-package serialization ──
    #[error("Key package TLS codec error: {0}")]
    KeyPackageTls(TlsCodecError),

    // ── MLS wire format (not parameterised over storage) ──
    #[error(transparent)]
    MlsMessageDeserialize(#[from] openmls::prelude::Error),

    #[error(transparent)]
    ProtocolMessage(#[from] openmls::framing::errors::ProtocolMessageError),

    #[error(transparent)]
    MlsMessageSerialize(#[from] openmls::framing::errors::MlsMessageError),

    // ── MLS group operations ──
    #[error(transparent)]
    ProcessMessage(BoxedError),

    #[error(transparent)]
    CreateMessage(#[from] CreateMessageError),

    #[error(transparent)]
    MergeCommit(BoxedError),

    #[error(transparent)]
    MergePendingCommit(BoxedError),

    #[error(transparent)]
    BuildCommit(BoxedError),

    #[error(transparent)]
    NewGroup(BoxedError),

    #[error(transparent)]
    Welcome(BoxedError),

    // ── Storage backend  ──
    #[error(transparent)]
    MlsStorage(BoxedError),

    // ── Semantic ──
    #[error("No pending staged commit for group: {0}")]
    NoPendingStagedCommit(String),

    #[error("Leaf index {0} resolves to no active member in the group")]
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

impl From<CreateCommitError> for MlsError {
    fn from(e: CreateCommitError) -> Self {
        MlsError::BuildCommit(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<CommitBuilderStageError<E>> for MlsError {
    fn from(e: CommitBuilderStageError<E>) -> Self {
        MlsError::BuildCommit(Box::new(e))
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
