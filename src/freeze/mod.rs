//! Freeze round candidate processing, selection, and commit application.
//!
//! Two submodules:
//! - `round` — public surface plus per-round setup and priority selection.
//! - `apply` — phase-3 loop: per-candidate apply, staging, validation,
//!   and post-commit bookkeeping.

mod apply;
mod round;

pub use round::{
    CommitHash, FreezeFinalizeResult, FreezeOutcome, buffer_commit_candidate, compute_commit_hash,
    finalize_freeze_round, replay_early_candidates,
};
