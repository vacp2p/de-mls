//! Per-conversation core types: protocol state, MLS-bound aggregate,
//! state machine, and durable config.
//!
//! Layout:
//! - [`group`] — the [`Group`] struct + protocol queues + dedup caches.
//! - [`handle`] — [`GroupHandle`] (data + protocol-function wrappers,
//!   owned by the orchestrator).
//! - [`state_machine`] — passive state enum + named transitions.
//! - [`config`] — durable timing/protocol config.

mod config;
#[allow(clippy::module_inception)]
mod group;
mod handle;
mod state_machine;

pub use config::{
    DEFAULT_COMMIT_INACTIVITY_DURATION, DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_ELECTION_VOTING_DELAY,
    DEFAULT_LIVENESS_CRITERIA_YES, DEFAULT_PEER_SCORE, DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
    DEFAULT_PROPOSAL_EXPIRATION, DEFAULT_RECOVERY_INACTIVITY_DURATION, DEFAULT_VOTING_DELAY,
    GroupConfig,
};
pub use group::{
    BufferedCommitCandidate, Group, PendingUpdate, ProposalId, member_set, self_leave_proposal_id,
    target_identity_of,
};
pub use handle::GroupHandle;
pub use state_machine::{GroupState, GroupStateMachine, OperatingMode};
