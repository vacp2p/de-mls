//! App-side per-conversation runner: wraps a [`crate::core::ConversationHandle`]
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
mod consensus_events;
mod freeze;
mod inbound;
mod lock;
mod messaging;
mod query;
mod runner;
mod steward;

pub use consensus::CreatorVote;
pub use freeze::PendingJoinTick;
pub use inbound::DispatchOutcome;
pub(crate) use lock::LockExt;
pub use messaging::build_key_package_message;
pub use runner::SessionRunner;
