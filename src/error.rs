use std::string::FromUtf8Error;

use openmls::{
    framing::errors::MlsMessageError,
    group::ProposeRemoveMemberError,
    prelude::{
        CommitToPendingProposalsError, CreateMessageError, MergeCommitError,
        MergePendingCommitError, NewGroupError, ProcessMessageError, ProposeAddMemberError,
        RemoveMembersError,
    },
};
use openmls_rust_crypto::MemoryStorageError;

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("Failed to verify signature: {0}")]
    InvalidSignature(#[from] libsecp256k1::Error),
    #[error("JSON processing error: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("Failed to serialize or deserialize MLS message: {0}")]
    InvalidMlsMessage(#[from] MlsMessageError),
}

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error(transparent)]
    MessageError(#[from] MessageError),

    #[error("Steward not set")]
    StewardNotSet,
    #[error("MLS group not initialized")]
    MlsGroupNotSet,
    #[error("Group still active")]
    GroupStillActive,
    #[error("Empty welcome message")]
    EmptyWelcomeMessage,
    #[error("Member not found")]
    MemberNotFound,

    #[error("Unable to create MLS group: {0}")]
    UnableToCreateGroup(#[from] NewGroupError<MemoryStorageError>),
    #[error("Unable to merge pending commit in MLS group: {0}")]
    UnableToMergePendingCommit(#[from] MergePendingCommitError<MemoryStorageError>),
    #[error("Unable to merge staged commit in MLS group: {0}")]
    UnableToMergeStagedCommit(#[from] MergeCommitError<MemoryStorageError>),
    #[error("Unable to process message: {0}")]
    InvalidProcessMessage(#[from] ProcessMessageError),
    #[error("Unable to encrypt MLS message: {0}")]
    UnableToEncryptMlsMessage(#[from] CreateMessageError),
    #[error("Unable to remove members: {0}")]
    UnableToRemoveMembers(#[from] RemoveMembersError<MemoryStorageError>),
    #[error("Unable to create proposal to add members: {0}")]
    UnableToCreateProposal(#[from] ProposeAddMemberError<MemoryStorageError>),
    #[error("Unable to create proposal to remove members: {0}")]
    UnableToCreateProposalToRemoveMembers(#[from] ProposeRemoveMemberError<MemoryStorageError>),
    #[error("Unable to revert commit to pending proposals: {0}")]
    UnableToRevertCommitToPendingProposals(
        #[from] CommitToPendingProposalsError<MemoryStorageError>,
    ),

    #[error("Failed to serialize mls message: {0}")]
    MlsMessageError(#[from] MlsMessageError),

    #[error("UTF-8 parsing error: {0}")]
    Utf8ParsingError(#[from] FromUtf8Error),
    #[error("JSON processing error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] tls_codec::Error),
    #[error("Failed to decode app message: {0}")]
    AppMessageDecodeError(String),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
