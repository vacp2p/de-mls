//! Core API for MLS group operations.
//!
//! All functions operate on a [`crate::core::Group`] (app-level state) plus
//! [`crate::mls_crypto::OpenMlsService`] (MLS crypto). Lifecycle, message
//! building, inbound routing, steward candidate creation, and freeze
//! finalization are grouped by submodule.

mod freeze;
mod inbound;
mod lifecycle;
mod steward;

pub use freeze::{FreezeFinalizeResult, FreezeOutcome, finalize_freeze_round};
pub(crate) use freeze::{compute_commit_hash, process_commit_candidate};
pub use inbound::process_inbound;
pub use lifecycle::{
    build_create_proposal_request, build_key_package_message, build_message, create_group,
    prepare_to_join,
};
pub use steward::{
    ElectionDecision, create_commit_candidate, evaluate_election_initiation, group_members,
    is_deadlock_ecp_proposer,
};
