//! Peer-scoring vocabulary, traits, reference [`PeerScoringService`],
//! and score-derivation helpers.
//!
//! Submodules:
//! - [`types`] — `ScoreEvent`, `ScoreOp`, `ScoringConfig`, `PeerScoringEvent`,
//!   `ScoreSnapshot`, `ScoringMemberDiff` + RFC defaults.
//! - [`storage`] — `PeerScoreStorage` trait (concrete backends live in the
//!   app layer).
//! - [`plugin`] — `PeerScoringPlugin` trait (per-conversation contract).
//! - [`service`] — `PeerScoringService` reference implementation.
//! - [`helpers`] — `scoring_member_diff`, `emergency_score_ops` (pure
//!   functions over the protobuf wire types).

mod helpers;
mod plugin;
mod service;
mod storage;
mod types;

pub use helpers::{emergency_score_ops, scoring_member_diff};
pub use plugin::PeerScoringPlugin;
pub use service::PeerScoringService;
pub use storage::PeerScoreStorage;
pub use types::{
    DEFAULT_PEER_SCORE, DEFAULT_THRESHOLD_PEER_SCORE, PeerScoringEvent, ScoreEvent, ScoreOp,
    ScoreSnapshot, ScoringConfig, ScoringMemberDiff, default_score_deltas,
};
