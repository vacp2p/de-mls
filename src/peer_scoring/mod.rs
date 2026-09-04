//! Peer-scoring vocabulary, the library-owned [`PeerScoringService`], and
//! score-derivation helpers.
//!
//! Submodules:
//! - `types` — `ScoreEvent`, `ScoreOp`, `ScoringConfig`, `ScoreSnapshot`,
//!   `ScoreChange` (what a score move reports), `ScoringMemberDiff` + RFC defaults.
//! - `service` — `PeerScoringService`, the library-owned scoring logic and
//!   its score table.
//! - `helpers` — pure functions: `scoring_member_diff` (scoring-table vs.
//!   member-set diff) and `emergency_score_ops` (emergency-vote → score ops).

mod helpers;
mod service;
mod types;

pub(crate) use helpers::{emergency_score_ops, scoring_member_diff};
pub use service::PeerScoringService;
pub use types::{
    DEFAULT_PEER_SCORE, DEFAULT_THRESHOLD_PEER_SCORE, ScoreEvent, ScoringConfig,
    default_score_deltas,
};
pub(crate) use types::{ScoreChange, ScoreOp, ScoreSnapshot, ScoringMemberDiff};
