//! [`Error`] — everything the reference implementation can fail with.
//!
//! Variants that wrap an OpenMLS error generic over the storage backend
//! ([`ProcessMessageError`], [`WelcomeError`], …) hold it boxed, so one error
//! type flows regardless of which provider the group runs over.

use std::error::Error as StdError;

use openmls::prelude::tls_codec::Error as TlsCodecError;
use openmls::{
    group::{CommitBuilderStageError, CreateCommitError, NewGroupError, WelcomeError},
    prelude::{CreateMessageError, MergeCommitError, MergePendingCommitError, ProcessMessageError},
};

/// Boxed `std::error::Error` used as the payload of any variant whose source
/// type is generic over the storage backend.
pub type BoxedError = Box<dyn StdError + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // ── Crypto + key-package serialization ──
    #[error("key package TLS codec error: {0}")]
    KeyPackageTls(TlsCodecError),

    #[error("key package failed MLS validation")]
    KeyPackageInvalid(BoxedError),

    #[error("key package carries signature key {found:?}, the action names {expected:?}")]
    KeyPackageIdentityMismatch { expected: Vec<u8>, found: Vec<u8> },

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

    // ── Storage backend ──
    #[error(transparent)]
    MlsStorage(BoxedError),

    // ── Rejections and semantic failures ──
    #[error("message is for group {found:?}, this group is {expected:?}")]
    WrongGroup { expected: Vec<u8>, found: Vec<u8> },

    #[error("commit is from epoch {commit}, the group is at {current}")]
    StaleCommit { commit: u64, current: u64 },

    #[error("the bytes staged are not a commit")]
    NotACommit,

    #[error("commit sender is not a group member")]
    UnauthenticatedSender,

    #[error("no commit is staged or pending under that hash")]
    UnknownCommit,

    #[error("leaf index {0} resolves to no active member in the group")]
    UnknownLeafIndex(u32),

    #[error("no group is stored for conversation {0}")]
    GroupNotFound(String),
}

impl Error {
    /// Box any storage-backend error into [`Error::MlsStorage`]. Used at call
    /// sites whose error type is the storage's concrete `Error`, which cannot
    /// be auto-converted through a blanket `From<E>` without colliding with
    /// the impls below.
    pub fn storage<E: StdError + Send + Sync + 'static>(e: E) -> Self {
        Error::MlsStorage(Box::new(e))
    }
}

// ── Boxing From impls for storage-parameterised OpenMLS error types ──
//
// Each impl is generic over `E: StdError + Send + Sync + 'static` so any
// storage backend's error type works.

impl<E: StdError + Send + Sync + 'static> From<ProcessMessageError<E>> for Error {
    fn from(e: ProcessMessageError<E>) -> Self {
        Error::ProcessMessage(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<MergeCommitError<E>> for Error {
    fn from(e: MergeCommitError<E>) -> Self {
        Error::MergeCommit(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<MergePendingCommitError<E>> for Error {
    fn from(e: MergePendingCommitError<E>) -> Self {
        Error::MergePendingCommit(Box::new(e))
    }
}

impl From<CreateCommitError> for Error {
    fn from(e: CreateCommitError) -> Self {
        Error::BuildCommit(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<CommitBuilderStageError<E>> for Error {
    fn from(e: CommitBuilderStageError<E>) -> Self {
        Error::BuildCommit(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<NewGroupError<E>> for Error {
    fn from(e: NewGroupError<E>) -> Self {
        Error::NewGroup(Box::new(e))
    }
}

impl<E: StdError + Send + Sync + 'static> From<WelcomeError<E>> for Error {
    fn from(e: WelcomeError<E>) -> Self {
        Error::Welcome(Box::new(e))
    }
}
