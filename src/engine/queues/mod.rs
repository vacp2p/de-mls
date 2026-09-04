//! Per-conversation protocol-queue state: the `EngineQueues` struct, its
//! settled-membership bookkeeping, and the urgent-commit target live in
//! [`state`]. The approved/voting proposal queues live in [`proposals`],
//! extending `EngineQueues` with their own `impl` block.

mod proposals;
mod state;

#[cfg(test)]
pub(crate) use state::BoundedSet;
pub(crate) use state::VotingMeta;
pub use state::{EngineQueues, ProposalId};

#[cfg(test)]
mod tests;
