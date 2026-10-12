//! Library error type: [`ConversationError`], raised by engine operations.

use hashgraph_like_consensus::error::ConsensusError;

/// Why an input was refused. A refusal leaves no trace in the engine; the
/// router feeds the input again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The conversation is `Syncing`: a candidate cannot be judged yet.
    Syncing,
    /// No session is held for the proposal.
    UnknownSession { proposal_id: u32 },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syncing => write!(f, "no steward list covers this epoch"),
            Self::UnknownSession { proposal_id } => {
                write!(f, "no session for proposal {proposal_id}")
            }
        }
    }
}

/// Errors from operations on a single conversation — protocol faults and
/// caller-facing guard failures alike.
///
/// Registry-level failures (conversation lookup, lock poisoning, transport
/// delivery) are integrator concerns and live in the integrator's own error
/// type.
#[derive(Debug, thiserror::Error)]
pub enum ConversationError {
    #[error("refused: {0}")]
    Refused(Refusal),

    #[error(
        "Lower-priority proposal blocked: an emergency criteria proposal is active (RFC partial freeze)"
    )]
    PartialFreeze,

    #[error("Consensus error: {0}")]
    Consensus(#[from] ConsensusError),

    #[error("Message error: {0}")]
    Message(#[from] prost::DecodeError),

    #[error("No proposals available")]
    NoProposals,

    #[error("Invalid conversation update request")]
    InvalidConversationUpdateRequest,

    #[error("Empty members list")]
    EmptyMembersList,

    #[error("Member is no longer in the group")]
    MemberGone,

    #[error("Key package belongs to a member already in the group")]
    AlreadyMember,

    #[error("Invalid config size")]
    InvalidConfigSize,

    #[error("Invalid conversation config: {0}")]
    InvalidConfig(String),

    #[error("Non-MLS proposals found in approved queue (ids: {proposal_ids:?})")]
    UnexpectedNonMlsProposals { proposal_ids: Vec<u32> },

    #[error("Store error: {0}")]
    Store(String),
}
