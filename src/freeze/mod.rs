//! Freeze round candidate processing, selection, and commit application.
//!
//! Submodules:
//! - `round` — public surface (buffer/replay/finalize) and priority selection.
//! - `context` — the pre-merge `RoundContext` snapshot that apply reads from.
//! - `apply` — per-candidate apply, staging, validation, post-commit bookkeeping.

mod apply;
mod context;
mod round;

pub use round::{
    CommitHash, FreezeFinalizeResult, FreezeOutcome, buffer_commit_candidate, compute_commit_hash,
    finalize_freeze_round, replay_early_candidates,
};
