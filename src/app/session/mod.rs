//! App-side per-conversation runner: wraps a [`crate::core::Conversation`]
//! together with a [`crate::app::PhaseTimer`] and the per-proposal auto-vote
//! timer registry. Coordinator methods compose state-machine transitions
//! with phase-timer anchors so callers update both in one call.
//!
//! [`SessionRunner`] is defined in [`runner`]; per-conversation method
//! bodies (proposal submission, voting, inbound dispatch, freeze ticks,
//! steward housekeeping, query getters) live in sibling modules and extend
//! `SessionRunner` via additional `impl` blocks.

mod consensus;
mod consensus_bridge;
mod consensus_context;
mod consensus_events;
mod construct;
mod freeze;
mod inbound;
mod messaging;
mod query;
mod runner;
mod steward;
mod tick;

pub use consensus::CreatorVote;
pub use consensus_context::ConsensusContext;
pub use construct::ConversationDeps;
pub use freeze::PendingJoinTick;
pub use inbound::DispatchOutcome;
pub use messaging::Outbound;
pub use runner::SessionRunner;
pub use tick::SessionTick;
