//! Commit round candidate processing, selection, and commit application.
//!
//! Submodules:
//! - `buffer` — the per-epoch candidate buffer (`CommitRoundBuffer`) and its types.
//! - `round` — public surface (buffer/replay/finalize) and priority selection.
//! - `context` — the pre-merge `RoundContext` snapshot that apply reads from.
//! - `apply` — the best-first apply loop and its local/remote apply paths.
//! - `validate` — commit validation service: stage, authorize, action-match.

mod apply;
mod buffer;
mod context;
mod round;
mod validate;

pub use buffer::{BufferedCommitCandidate, CommitBufferOutcome, CommitRoundBuffer};
pub use round::{
    CommitHash, CommitRoundOutcome, CommitRoundResult, compute_commit_hash, finalize_commit_round,
    receive_commit_candidate, replay_early_candidates,
};
