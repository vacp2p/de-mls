//! Steward list: the library-owned deterministic list and rotation queries.
//!
//! - `list` — [`StewardList`] value and [`StewardListConfig`]
//! - `service` — [`StewardListService`], the stateful per-conversation steward list

mod list;
mod service;

pub use list::{StewardList, StewardListConfig};
pub use service::{
    DEFAULT_MAX_RETRIES, ElectionDecision, ElectionSkip, StewardListService, StewardListSnapshot,
};
