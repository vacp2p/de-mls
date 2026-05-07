//! Core API for MLS group operations.
//!
//! Functions operate on [`crate::core::Group`] (which owns its
//! [`crate::mls_crypto::MlsService`]). Submodules group inbound routing,
//! freeze finalization, steward candidate creation, and the welcome- /
//! consensus-message framing helpers.

mod freeze;
mod inbound;
mod lifecycle;
mod steward;

pub use freeze::{FreezeFinalizeResult, FreezeOutcome, finalize_freeze_round};
pub use freeze::{compute_commit_hash, process_commit_candidate};
pub use inbound::process_inbound;
pub use lifecycle::{build_create_proposal_request, build_key_package_message};
