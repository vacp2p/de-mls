//! Steward list: the library-owned deterministic list and rotation queries.
//!
//! - `list` — the `StewardList` value and its [`StewardListConfig`] bounds
//! - `service` — the per-conversation list state

mod list;
mod service;

pub(crate) use list::StewardList;
pub use list::StewardListConfig;
pub(crate) use service::{ElectionDecision, ElectionSkip, StewardListService};
