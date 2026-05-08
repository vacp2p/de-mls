//! Steward list plug-in: passive, per-group steward roster.
//!
//! Answers "who is the steward at epoch N?", "am I a steward?", and
//! "is the list exhausted?". Stores its own list, election epoch, retry
//! round, and retry policy. Never reaches into MLS, consensus, or other
//! plug-ins — the coordinator composes the eligibility predicate (MLS
//! membership minus pending-removal proposals) and passes it on every
//! position query.
//!
//! Mutators return [`StewardListEvent`]s so the coordinator can chain
//! protocol actions (broadcast `GroupSync` after install, escalate to a
//! Layer-3 `Deadlock` ECP after retry exhaustion).
//!
//! Layout:
//! - [`list`] — the deterministic [`StewardList`] data type and config.
//! - [`plugin`] — the [`StewardListPlugin`] trait + event vocabulary.
//! - [`deterministic`] — reference impl ([`DeterministicStewardList`]).

mod deterministic;
mod list;
mod plugin;

pub use deterministic::DeterministicStewardList;
pub use list::{StewardList, StewardListConfig};
pub use plugin::{DEFAULT_MAX_RETRIES, ElectionDecision, StewardListEvent, StewardListPlugin};
