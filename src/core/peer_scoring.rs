//! Types and traits core uses to describe scoring-relevant events. The
//! scoring service itself lives in `crate::app::peer_scoring` and consumes
//! the `Vec<ScoreOp>` that core functions return.

// ── Score events ────────────────────────────────────────────────────

/// A scoreable event in the protocol.
///
/// Each variant maps to a single score delta. Violation types (BrokenCommit, etc.)
/// go through the ECP consensus path — when accepted, the target receives a
/// violation-type-specific penalty and the creator receives a flat reward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScoreEvent {
    // ── ECP target penalties (mapped from ViolationType in evidence) ──
    /// ECP accepted: steward committed proposals that don't match what was voted on.
    BrokenCommit,
    /// ECP accepted: MLS proposal payload was malformed or didn't match the voted action.
    BrokenMlsProposal,
    /// ECP accepted: steward failed to commit within the threshold duration.
    CensorshipInactivity,

    // ── ECP creator outcomes ──
    /// ECP accepted — flat reward to the proposal creator.
    EmergencyYesCreator,
    /// ECP rejected — flat penalty to the proposal creator (false accusation).
    EmergencyNoCreator,

    // ── Commit selection ──
    /// Steward successfully committed a valid batch.
    SuccessfulCommit,
    /// Competing commit with same proposals but different MLS entropy — honest
    /// participation (RFC: "MUST NOT be classified as misbehavior").
    HonestCommitAttempt,
    /// Competing commit with a different proposal set than the selected one
    /// (RFC: "MUST be classified as misbehavior").
    MisbehavingCommit,
}

/// A score operation produced by core logic and fed into the app's
/// [`PeerScoringService`](crate::app::PeerScoringService).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreOp {
    pub member_id: Vec<u8>,
    pub event: ScoreEvent,
}

// ── Scoring configuration ───────────────────────────────────────────

/// Maps each [`ScoreEvent`] to a signed score delta (positive = reward).
/// Default impl: [`FixedScoringProvider`](crate::app::FixedScoringProvider).
pub trait ScoringProvider {
    fn score_delta(&self, event: ScoreEvent) -> i64;
}

#[derive(Debug, Clone)]
pub struct ScoringConfig {
    /// Score assigned to newly added members.
    pub default_score: i64,
}

/// Per-(group, member) score persistence. Default impl:
/// [`InMemoryPeerScoreStorage`](crate::app::InMemoryPeerScoreStorage).
pub trait PeerScoreStorage {
    fn get(&self, group_id: &str, member_id: &[u8]) -> Option<i64>;
    fn set(&mut self, group_id: &str, member_id: &[u8], score: i64);
    fn remove(&mut self, group_id: &str, member_id: &[u8]);
    fn all_scores(&self, group_id: &str) -> Vec<(Vec<u8>, i64)>;

    /// Drop every score entry for `group_id`. Called on leave so a future
    /// rejoin starts from a clean per-group table populated by the new
    /// `GroupSync` rather than carrying stale entries from the prior
    /// session.
    fn remove_group(&mut self, group_id: &str);
}
