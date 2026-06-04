//! Steward list plug-in: deterministic roster and rotation queries.
//!
//! Passive per-conversation state — no MLS or consensus I/O. The coordinator
//! passes an `eligible` predicate on every live position query.
//!
//! - [`list`] — [`StewardList`] and [`StewardListConfig`]
//! - [`plugin`] — [`StewardListPlugin`] trait and events
//! - [`deterministic`] — [`DeterministicStewardList`] reference impl

mod deterministic;
mod list;
mod plugin;

pub use deterministic::DeterministicStewardList;
pub use list::{StewardList, StewardListConfig};
pub use plugin::{DEFAULT_MAX_RETRIES, ElectionDecision, StewardListPlugin};
