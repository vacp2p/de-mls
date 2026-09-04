//! The [`Engine`] handle: the struct, its construction and queries, the
//! per-call output accumulator, and scoring, in [`engine`]. The
//! state-machine transitions and phase-timer bookkeeping live in
//! [`lifecycle`]; the next-wakeup computation and deadline registry in
//! [`wakeup`].

mod engine;
mod lifecycle;
mod wakeup;

pub use engine::Engine;
pub(crate) use engine::{AutoVoteEntry, PendingBuild, PendingMerge};
