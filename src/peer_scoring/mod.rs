//! Peer-scoring vocabulary, the [`PeerScoreStorage`] backend trait, the
//! library-owned [`PeerScoringService`], and score-derivation helpers.
//!
//! Submodules:
//! - `types` — `ScoreEvent`, `ScoreOp`, `ScoringConfig`,
//!   `ScoreSnapshot`, `ScoringMemberDiff` + RFC defaults.
//! - `storage` — `PeerScoreStorage` trait (integrator-supplied backend).
//! - `service` — `PeerScoringService`, the library-owned scoring logic.
//! - `helpers` — pure functions: `scoring_member_diff` (scoring-table vs.
//!   MLS-roster diff) and `emergency_score_ops` (emergency-vote → score ops).

mod helpers;
mod service;
mod storage;
mod types;

pub use helpers::{emergency_score_ops, scoring_member_diff};
pub use service::PeerScoringService;
pub use storage::PeerScoreStorage;
pub use types::{
    DEFAULT_PEER_SCORE, DEFAULT_THRESHOLD_PEER_SCORE, ScoreEvent, ScoreOp, ScoreSnapshot,
    ScoringConfig, ScoringMemberDiff, default_score_deltas,
};
