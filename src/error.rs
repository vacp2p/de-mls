//! Library error type: [`ConversationError`], raised by conversation operations.

use hashgraph_like_consensus::error::ConsensusError;

use crate::mls_crypto::MlsError;

/// Errors from operations on a single conversation — protocol faults and
/// caller-facing guard failures alike.
///
/// Registry-level failures (conversation lookup, lock poisoning, transport
/// delivery) are integrator concerns and live in the integrator's own error
/// type — see the reference `User` in the gateway crate.
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("Cannot send message: conversation is in {0} state")]
    ConversationBlocked(String),

    #[error(
        "Lower-priority proposal blocked: an emergency criteria proposal is active (RFC partial freeze)"
    )]
    PartialFreeze,

    #[error("MLS error: {0}")]
    Mls(#[from] MlsError),

    #[error("Consensus error: {0}")]
    Consensus(#[from] ConsensusError),

    #[error("Message error: {0}")]
    Message(#[from] prost::DecodeError),

    #[error("Score storage error: {0}")]
    ScoreStorage(Box<dyn std::error::Error + Send + Sync>),

    #[error("Caller is not a steward")]
    NotASteward,

    #[error("Not in Layer-3 recovery mode")]
    NotInRecovery,

    #[error("No proposals available")]
    NoProposals,

    #[error("Invalid conversation update request")]
    InvalidConversationUpdateRequest,

    #[error("Empty members list")]
    EmptyMembersList,

    #[error("Member is no longer in the group")]
    MemberGone,

    #[error("Invalid config size")]
    InvalidConfigSize,

    #[error("Invalid conversation config: {0}")]
    InvalidConfig(String),

    #[error("Non-MLS proposals found in approved queue (ids: {proposal_ids:?})")]
    UnexpectedNonMlsProposals { proposal_ids: Vec<u32> },
}
