//! Consensus integration, by file:
//!
//! - `plugin` — the [`ConsensusPlugin`] contract the integrator implements.
//! - `outcome_bus` — the channel the consensus service publishes resolved
//!   outcomes into, drained once per poll.
//! - `voting` — opening proposals, casting votes, leaving, and the poll tick
//!   that fires deadlines and drains the bus.
//! - `apply_result` — `apply_consensus_result`: how a resolved proposal
//!   reshapes the proposal queues.
//! - `handle_outcome` — `handle_consensus_outcome`: the follow-up that runs
//!   once a proposal resolves.

mod apply_result;
mod handle_outcome;
pub(crate) mod outcome_bus;
mod plugin;
mod voting;

pub(crate) use apply_result::{ConsensusApplyResult, apply_consensus_result};
pub(crate) use plugin::ConsensusEngine;
pub use plugin::ConsensusPlugin;
pub use voting::CreatorVote;
