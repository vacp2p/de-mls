//! Per-conversation protocol-queue state: the `EngineQueues` struct, its
//! settled-membership bookkeeping, and the committed-hash dedup window live
//! in [`state`]. The approved/voting proposal queues and the emergency set
//! live in [`proposals`]; the pending-update buffer and the urgent-commit
//! target in [`updates`] — both extend `EngineQueues` with their own `impl`
//! block.

mod proposals;
mod state;
mod updates;

#[cfg(test)]
pub(crate) use state::BoundedSet;
pub(crate) use state::VotingMeta;
pub use state::{EngineQueues, PendingUpdate, ProposalId};

#[cfg(test)]
mod tests;
