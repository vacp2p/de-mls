//! The policy engine: facts in, [`Output`](crate::engine::Output) out, no MLS inside.
//!
//! A libchat-side router owns the MLS group, opens every frame, hands the
//! engine control bytes with the MLS-authenticated sender, and executes the
//! decisions the engine returns. The design lives in
//! `docs/router-split-design.md`.

mod actions;
mod commit;
mod consensus_signer;
mod construct;
mod handle;
mod inbound;
mod outcome;
mod queues;
mod steward;
mod store;
mod tick;
mod types;
mod util;
mod voting;

pub use handle::Engine;
pub use store::{EngineStore, InMemoryStore, keys};
pub use types::{
    Action, Admission, CommitHash, Decision, DecisionFailure, Event, MemberId, MembershipDelta,
    Outbound, Output, Phase, StagedFacts, Timestamp,
};
