//! Error type for MLS operations.
//!
//! All MLS-side errors live in a single [`MlsError`] enum. Storage-, service-,
//! and identity-flavored variants sit at the same level — readers don't have
//! to peel back nested error types to see what failed.

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

/// All MLS-side errors.
#[derive(Debug, thiserror::Error)]
pub enum MlsError {
    // ── Identity ──
    /// Failed to build a key package.
    #[error(transparent)]
    UnableToCreateKeyPackage(#[from] KeyPackageNewError),

    /// Hash-ref derivation failed for a key package.
    #[error("Invalid hash reference: {0}")]
    InvalidHashRef(#[from] LibraryError),

    /// Failed to generate a fresh signature keypair.
    #[error("Failed to create signer: {0}")]
    UnableToCreateSigner(#[from] CryptoError),

    /// JSON serialization of a key package or other identity-adjacent value
    /// failed. Used for outbound serde paths; inbound key-package decoding
    /// goes through [`MlsError::KeyPackageJson`].
    #[error("JSON encoding error: {0}")]
    InvalidJson(serde_json::Error),

    /// Wallet address string did not parse as a 20-byte hex address.
    #[error("Invalid wallet address: {0}")]
    InvalidWalletAddress(String),

    // ── MLS wire format ──
    /// Failed to deserialize an inbound MLS message wire frame.
    #[error("Failed to deserialize MLS message: {0}")]
    MlsMessageDeserialize(#[from] openmls::prelude::Error),

    /// Failed to extract the protocol message from a deserialized envelope.
    #[error("Failed to convert to protocol message: {0}")]
    ProtocolMessage(#[from] openmls::framing::errors::ProtocolMessageError),

    /// Failed to serialize an outbound MLS message.
    #[error("Failed to serialize MLS message: {0}")]
    MlsMessageSerialize(#[from] openmls::framing::errors::MlsMessageError),

    /// JSON decoding of an inbound key-package payload failed.
    #[error("Invalid key package bytes: {0}")]
    KeyPackageJson(serde_json::Error),

    // ── MLS group operations ──
    /// OpenMLS rejected an inbound message during processing.
    #[error("Failed to process MLS message: {0}")]
    ProcessMessage(#[from] ProcessMessageError<MemoryStorageError>),

    /// Encrypting/serializing an outbound MLS application message failed.
    #[error("Failed to create MLS message: {0}")]
    CreateMessage(#[from] CreateMessageError),

    /// Merging a previously-staged commit failed.
    #[error("Failed to merge staged commit: {0}")]
    MergeCommit(#[from] MergeCommitError<MemoryStorageError>),

    /// Merging the local pending commit failed.
    #[error("Failed to merge pending commit: {0}")]
    MergePendingCommit(#[from] MergePendingCommitError<MemoryStorageError>),

    /// Building the commit out of pending proposals failed.
    #[error("Failed to commit to pending proposals: {0}")]
    CommitToPendingProposals(#[from] CommitToPendingProposalsError<MemoryStorageError>),

    /// Building an Add proposal failed.
    #[error("Failed to propose add member: {0}")]
    ProposeAddMember(#[from] ProposeAddMemberError<MemoryStorageError>),

    /// Building a Remove proposal failed.
    #[error("Failed to propose remove member: {0}")]
    ProposeRemoveMember(#[from] ProposeRemoveMemberError<MemoryStorageError>),

    /// Creating a fresh MLS group failed.
    #[error("Failed to create MLS group: {0}")]
    NewGroup(#[from] NewGroupError<MemoryStorageError>),

    /// Joining an MLS group from a welcome failed.
    #[error("Failed to join MLS group: {0}")]
    Welcome(#[from] WelcomeError<MemoryStorageError>),

    // ── Storage / locking ──
    /// OpenMLS storage backend reported a failure.
    #[error("MLS storage error: {0}")]
    MlsStorage(#[from] MemoryStorageError),

    /// Synchronization-primitive lock recovered from a poison.
    #[error("Lock poisoned: {0}")]
    Lock(String),

    // ── Semantic ──
    /// Wire-level message kind didn't match the lane it arrived on.
    #[error("Unexpected MLS message type")]
    UnexpectedMessageType,

    /// No local MLS state for the requested group id.
    #[error("Group not found: {0}")]
    GroupNotFound(String),

    /// Welcome did not address one of our key packages.
    #[error("Welcome message not for this user")]
    WelcomeNotForUs,

    /// Caller asked to merge/discard a staged commit but none was staged.
    #[error("No pending staged commit for group: {0}")]
    NoPendingStagedCommit(String),

    /// Remove proposal references a leaf index that is not currently present.
    #[error("Remove proposal references leaf index {0} with no active credential")]
    UnknownLeafIndex(u32),
}
