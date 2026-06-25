//! Steward list: the library-owned deterministic roster and rotation queries.
//!
//! - `list` — [`StewardList`] value and [`StewardListConfig`]
//! - `service` — [`StewardListService`], the stateful per-conversation roster

mod list;
mod service;

pub use list::{StewardList, StewardListConfig};
pub use service::{
    DEFAULT_MAX_RETRIES, ElectionDecision, ElectionSkip, StewardListService, StewardListSnapshot,
};
