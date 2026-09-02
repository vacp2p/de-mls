//! Steward list: the library-owned deterministic list and rotation queries.
//!
//! - `list` — the `StewardList` value and its [`StewardListConfig`] bounds
//! - `service` — the per-conversation list state, and the
//!   [`StewardListSnapshot`] an integrator persists

mod list;
mod service;

pub(crate) use list::StewardList;
pub use list::StewardListConfig;
pub use service::StewardListSnapshot;
pub(crate) use service::{DEFAULT_MAX_RETRIES, ElectionDecision, ElectionSkip, StewardListService};
