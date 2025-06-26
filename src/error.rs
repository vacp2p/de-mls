use alloy::signers::local::LocalSignerError;
use kameo::error::SendError;
use openmls::group::WelcomeError;
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
use std::{str::Utf8Error, string::FromUtf8Error};

use ds::{waku_actor::WakuMessageToSend, DeliveryServiceError};
use mls_crypto::error::IdentityError;

#[derive(Debug, thiserror::Error)]
pub enum ConsensusError {
    #[error("Invalid vote ID")]
    InvalidVoteId,
    #[error("Invalid vote hash")]
    InvalidVoteHash,
    #[error("Duplicate vote")]
    DuplicateVote,
    #[error("Session not active")]
    SessionNotActive,
    #[error("Invalid message")]
    InvalidMessage,
    #[error("Proposal not found")]
    ProposalNotFound,
    #[error("Vote validation failed")]
    VoteValidationFailed,
    #[error("Consensus timeout")]
    ConsensusTimeout,
    #[error("Insufficient votes for consensus")]
    InsufficientVotes,
    #[error("Invalid signature")]
    InvalidSignature,
    #[error("Proposal expired")]
    ProposalExpired,
    #[error("An unknown consensus error occurred: {0}")]
    Other(String),
}

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
    #[error("Invalid state transition")]
    InvalidStateTransition,
    #[error("Empty proposals for current epoch")]
    EmptyProposals,

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
    #[error("Unable to store pending proposal: {0}")]
    UnableToStorePendingProposal(#[from] MemoryStorageError),

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

    #[error("Group already exists: {0}")]
    GroupAlreadyExistsError(String),
    #[error("Group not found: {0}")]
    GroupNotFoundError(String),

    #[error("Unsupported message type.")]
    UnsupportedMessageType,
    #[error("Welcome message cannot be empty.")]
    EmptyWelcomeMessageError,
    #[error("Message verification failed")]
    MessageVerificationFailed,

    #[error("Unknown content topic type: {0}")]
    UnknownContentTopicType(String),

    #[error("Failed to create staged join: {0}")]
    MlsWelcomeError(#[from] WelcomeError<MemoryStorageError>),

    #[error("UTF-8 parsing error: {0}")]
    Utf8ParsingError(#[from] FromUtf8Error),
    #[error("UTF-8 string parsing error: {0}")]
    Utf8StringParsingError(#[from] Utf8Error),
    #[error("JSON processing error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] tls_codec::Error),
    #[error("Failed to parse signer: {0}")]
    SignerParsingError(#[from] LocalSignerError),

    #[error("Failed to publish message: {0}")]
    KameoPublishMessageError(#[from] SendError<WakuMessageToSend, DeliveryServiceError>),
    #[error("Failed to create group: {0}")]
    KameoCreateGroupError(String),
    #[error("Failed to send message to user: {0}")]
    KameoSendMessageError(String),
    #[error("Failed to get income key packages: {0}")]
    GetIncomeKeyPackagesError(String),
    #[error("Failed to process steward message: {0}")]
    ProcessStewardMessageError(String),
    #[error("Failed to process proposals: {0}")]
    ProcessProposalsError(String),
    #[error("Unsupported mls message type")]
    UnsupportedMlsMessageType,
    #[error("Failed to decode welcome message: {0}")]
    WelcomeMessageDecodeError(#[from] prost::DecodeError),
    #[error("Failed to apply proposals: {0}")]
    ApplyProposalsError(String),
    #[error("Failed to deserialize mls message in: {0}")]
    MlsMessageInDeserializeError(String),
    #[error("Failed to create invite proposal: {0}")]
    CreateInviteProposalError(String),
    #[error("Failed to try into protocol message: {0}")]
    TryIntoProtocolMessageError(String),
    #[error("Failed to get group update requests: {0}")]
    GetGroupUpdateRequestsError(String),

    #[error("Failed to send message to waku: {0}")]
    WakuSendMessageError(#[from] tokio::sync::mpsc::error::SendError<WakuMessageToSend>),
}
