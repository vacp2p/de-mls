//! Core API for MLS group operations.
//!
//! All functions operate on a [`crate::core::Group`] (app-level state) plus
//! [`crate::mls_crypto::MlsService`] (MLS crypto). Lifecycle, message
//! building, inbound routing, steward candidate creation, and freeze
//! finalization are grouped by submodule.

mod freeze;
mod inbound;
mod lifecycle;
mod steward;

pub use freeze::{
    FreezeFinalizeResult, FreezeOutcome, compute_commit_hash, finalize_freeze_round,
    process_commit_candidate,
};
pub use inbound::process_inbound;
pub use lifecycle::{build_key_package_message, build_message, create_group, prepare_to_join};
pub use steward::{create_commit_candidate, group_members};
