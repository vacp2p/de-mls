//! Consensus side of a session: opening proposals, voting, and applying
//! resolved outcomes.
//!
//! `voting` submits and votes, `events` applies outcomes drained from the
//! event bus, and `bridge` holds the stateless consensus-library adapters
//! both use.

pub(crate) mod bridge;
mod events;
mod voting;

pub use voting::CreatorVote;
