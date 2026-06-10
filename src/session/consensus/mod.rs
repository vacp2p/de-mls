//! Consensus-side session logic.
//!
//! - `voting` — proposal submission and voting ops on
//!   [`crate::session::SessionRunner`] (`initiate_proposal`, `vote`,
//!   deadline ticking, self-leave).
//! - `events` — dispatch of consensus outcomes drained from the event bus
//!   (`apply_consensus_outcome` and its branch handlers).
//! - `bridge` — stateless helpers translating session intents into
//!   consensus-library calls.
//! - `context` — [`ConsensusContext`], the shared service + storage bundle
//!   sessions are built from.

pub(crate) mod bridge;
mod context;
mod events;
mod voting;

pub use context::ConsensusContext;
pub use voting::CreatorVote;
