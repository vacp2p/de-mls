//! Per-conversation core types: protocol state, MLS-bound aggregate,
//! state machine, and durable config.
//!
//! Layout:
//! - [`queues`] — the [`ConversationQueues`] struct + protocol queues +
//!   dedup caches.
//! - [`conversation`] — [`ConversationCore`], the MLS-bound aggregate
//!   (queues + MLS service + plug-ins + state machine), owned by the
//!   orchestrator.
//! - [`state_machine`] — passive state enum + named transitions.
//! - [`config`] — durable timing/protocol config.

mod config;
#[allow(clippy::module_inception)]
mod conversation;
mod queues;
mod state_machine;
mod util;

pub use config::{
    ConversationConfig, DEFAULT_COMMIT_INACTIVITY_DURATION, DEFAULT_CONSENSUS_TIMEOUT,
    DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_LIVENESS_CRITERIA_YES,
    DEFAULT_PENDING_UPDATE_MAX_EPOCHS, DEFAULT_PROPOSAL_EXPIRATION,
    DEFAULT_RECOVERY_INACTIVITY_DURATION, DEFAULT_VOTING_DELAY,
};
pub use conversation::ConversationCore;
pub use queues::{BufferedCommitCandidate, ConversationQueues, FreezeBufferOutcome, ProposalId};
pub use state_machine::{ConversationState, ConversationStateMachine, OperatingMode};
pub use util::{member_set, self_leave_proposal_id, target_member_id_of};
