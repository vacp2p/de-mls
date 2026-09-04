//! The policy engine: facts in, [`Output`](crate::engine::Output) out, no MLS inside.
//!
//! An application-owned router owns the MLS group, opens every frame, hands
//! the engine control bytes with the MLS-authenticated sender, and executes
//! the decisions the engine returns. See the crate documentation for the
//! router loop.

mod actions;
mod commit;
mod config;
mod consensus_signer;
mod construct;
mod handle;
mod inbound;
mod outcome;
mod outcome_bus;
mod phase_timer;
mod proposal_kind;
mod queues;
mod state_machine;
mod steward;
mod store;
#[cfg(test)]
pub(crate) mod test_support;
mod tick;
mod timestamp;
mod types;
mod util;
mod voting;

pub use config::{
    DEFAULT_BACKUP_TAKEOVER_WINDOW, DEFAULT_COMMIT_BATCH_MAX, DEFAULT_COMMIT_BATCH_WINDOW,
    DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_DEDUP_WINDOW, DEFAULT_LIVENESS_CRITERIA_YES,
    DEFAULT_MAX_CONSENSUS_SESSIONS, DEFAULT_PENDING_UPDATE_MAX_EPOCHS, DEFAULT_PROPOSAL_EXPIRATION,
    DEFAULT_UNANSWERED_SYNC_ROUNDS, DEFAULT_VOTING_DELAY, EngineConfig,
};
pub use handle::Engine;
pub use store::{EngineStore, InMemoryStore, keys};
pub use timestamp::Timestamp;
pub use types::{
    Action, CommitHash, Decision, DecisionFailure, Event, MemberId, MembershipDelta, Outbound,
    Output, Phase, StagedFacts,
};
