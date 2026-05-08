//! Core API for MLS group operations.
//!
//! Submodules group inbound routing, freeze finalization, and the
//! welcome- / consensus-message framing helpers. Steward commit-candidate
//! creation lives on [`crate::core::GroupHandle::create_commit_candidate`].

mod freeze;
mod inbound;
mod lifecycle;

pub use freeze::{FreezeFinalizeResult, FreezeOutcome, finalize_freeze_round};
pub use freeze::{compute_commit_hash, process_commit_candidate};
pub use inbound::process_inbound;
pub use lifecycle::{build_create_proposal_request, build_key_package_message};
