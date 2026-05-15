//! Peer-scoring value types: events, ops, configuration, threshold-cross
//! events, snapshots, and the membership-diff result.

use std::collections::HashMap;

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

/// A score operation produced by core logic. The app layer feeds these
/// into its scoring service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreOp {
    pub member_id: Vec<u8>,
    pub event: ScoreEvent,
}

/// RFC §Peer Scoring default delta for each [`ScoreEvent`]. Values are
/// placeholders pending an empirical tuning pass (see `docs/ROADMAP.md`).
/// Integrators that want different deltas pass their own map to
/// [`super::PeerScoringService::new`].
pub fn default_score_deltas() -> HashMap<ScoreEvent, i64> {
    HashMap::from([
        // ECP target penalties (violation-type-specific)
        (ScoreEvent::BrokenCommit, -50),
        (ScoreEvent::BrokenMlsProposal, -30),
        (ScoreEvent::CensorshipInactivity, -40),
        // ECP creator outcomes
        (ScoreEvent::EmergencyYesCreator, 20),
        (ScoreEvent::EmergencyNoCreator, -50),
        // Commit selection
        (ScoreEvent::SuccessfulCommit, 10),
        (ScoreEvent::HonestCommitAttempt, 5),
        (ScoreEvent::MisbehavingCommit, -30),
    ])
}

/// RFC §Peer Scoring `default_peer_score`: starting score for a new member.
pub const DEFAULT_PEER_SCORE: i64 = 100;

/// Default removal threshold (RFC §Peer Scoring `threshold_peer_score`).
pub const DEFAULT_THRESHOLD_PEER_SCORE: i64 = 0;

#[derive(Debug, Clone)]
pub struct ScoringConfig {
    /// Score assigned to newly added members.
    pub default_score: i64,
    /// At or below this score, a member is eligible for
    /// `SCORE_BELOW_THRESHOLD` ECP removal (RFC §Peer Scoring).
    pub threshold: i64,
}

impl Default for ScoringConfig {
    /// RFC §Peer Scoring defaults. Adjust at `User` init via
    /// [`crate::app::User::set_default_scoring_config`].
    fn default() -> Self {
        Self {
            default_score: DEFAULT_PEER_SCORE,
            threshold: DEFAULT_THRESHOLD_PEER_SCORE,
        }
    }
}

/// Event emitted by a [`super::PeerScoringPlugin`] when an apply call moves
/// a member across the configured threshold. Plug-ins return events from
/// every mutating call; the coordinator drains them at known safe
/// points and turns them into protocol actions (e.g. submitting
/// `SCORE_BELOW_THRESHOLD`). The `score` field carries the post-apply
/// value at the time of the cross.
///
/// "Untracked → tracked" via [`super::PeerScoringPlugin::add_member`] or a
/// [`super::PeerScoringPlugin::apply_snapshot`] entry counts as crossing
/// from above for cross-detection — a fresh entry that lands at-or-below
/// threshold emits `ThresholdCrossedDown`. Coordinators should treat
/// the events the same regardless of source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerScoringEvent {
    /// Member's score moved from above-threshold to at-or-below threshold.
    ThresholdCrossedDown { member_id: Vec<u8>, score: i64 },
    /// Member's score moved from at-or-below threshold back above it.
    /// Reserved for future recovery-scoring use cases — coordinators
    /// today drop these silently.
    ThresholdCrossedUp { member_id: Vec<u8>, score: i64 },
}

/// Sparse snapshot of per-member scores for joiner bootstrap. Carries
/// only members whose score has diverged from `default_score`; the
/// recipient initialises every member at default via membership sync
/// before applying the snapshot, so missing entries imply default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScoreSnapshot {
    /// Members whose score `!= default_score`. Absolute scores, not deltas.
    pub diverged: Vec<(Vec<u8>, i64)>,
}

/// Members to add to and remove from a scoring table to bring it into
/// sync with the current MLS membership.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScoringMemberDiff {
    pub to_add: Vec<Vec<u8>>,
    pub to_remove: Vec<Vec<u8>>,
}
