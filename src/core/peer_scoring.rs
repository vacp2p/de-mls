//! Protocol vocabulary for peer scoring.
//!
//! This module defines the types and traits that core functions use to communicate
//! scoring-relevant events. The actual scoring service and default implementations
//! live in the `app::peer_scoring` module (re-exported via [`crate::app`]).
//!
//! # Data flow
//!
//! ```text
//! core functions ──► Vec<ScoreOp> ──► app layer ──► PeerScoringService
//! ```

// ── Score events ────────────────────────────────────────────────────

/// A scoreable event in the protocol.
///
/// This is a finite, predetermined catalog. Members are scored only on events
/// from this list — no free-form penalties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScoreEvent {
    /// Steward committed proposals that don't match what was voted on.
    BrokenCommit,
    /// MLS proposal payload was malformed or didn't match the voted action.
    BrokenMlsProposal,
    /// Steward failed to commit within the threshold duration.
    CensorshipInactivity,
    /// Steward successfully committed a valid batch.
    SuccessfulCommit,
    /// Emergency criteria accepted — penalty to the accused target.
    EmergencyYesTarget,
    /// Emergency criteria accepted — reward to the proposal creator.
    EmergencyYesCreator,
    /// Emergency criteria rejected — penalty to the proposal creator (false accusation).
    EmergencyNoCreator,
    /// Commit referenced proposals that were not yet finalized by consensus.
    NonFinalizedProposalCommit,
}

/// A score operation produced by core logic.
///
/// Core functions return these to describe scoring-relevant events. The application
/// layer feeds them into a [`PeerScoringService`](crate::app::PeerScoringService).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreOp {
    pub member_id: Vec<u8>,
    pub event: ScoreEvent,
}

impl ScoreOp {
    pub fn new(member_id: Vec<u8>, event: ScoreEvent) -> Self {
        Self { member_id, event }
    }
}

// ── Scoring configuration ───────────────────────────────────────────

/// Maps each [`ScoreEvent`] to a score delta.
///
/// Implement this trait to customize score values. A default implementation
/// ([`FixedScoringProvider`](crate::app::FixedScoringProvider)) is provided in the app layer.
pub trait ScoringProvider {
    /// Returns the score change for the given event.
    ///
    /// Positive values reward, negative values penalize.
    fn score_delta(&self, event: ScoreEvent) -> i64;
}

/// Configuration for a [`PeerScoringService`](crate::app::PeerScoringService).
#[derive(Debug, Clone)]
pub struct ScoringConfig {
    /// Score assigned to newly added members.
    pub default_score: i64,
    /// Members at or below this score are candidates for removal.
    pub removal_threshold: i64,
}

// ── Storage trait ───────────────────────────────────────────────────

/// Persistence layer for peer scores.
///
/// Scores are scoped by group — each `(group_id, member_id)` pair has its own
/// score. A default in-memory implementation
/// ([`InMemoryPeerScoreStorage`](crate::app::InMemoryPeerScoreStorage)) is
/// provided in the app layer. Implement this trait to back scores with a
/// database or other durable store.
pub trait PeerScoreStorage {
    fn get(&self, group_id: &str, member_id: &[u8]) -> Option<i64>;
    fn set(&mut self, group_id: &str, member_id: &[u8], score: i64);
    fn remove(&mut self, group_id: &str, member_id: &[u8]);
    fn all_scores(&self, group_id: &str) -> Vec<(Vec<u8>, i64)>;
}

// ── Result type for consensus ───────────────────────────────────────

/// Result of applying a consensus outcome, including any score events
/// that the application layer should feed into a [`PeerScoringService`](crate::app::PeerScoringService).
#[derive(Debug, Clone, Default)]
pub struct ConsensusApplyResult {
    pub score_ops: Vec<ScoreOp>,
}

impl ConsensusApplyResult {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with_ops(score_ops: Vec<ScoreOp>) -> Self {
        Self { score_ops }
    }
}
