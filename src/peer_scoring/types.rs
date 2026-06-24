//! Peer-scoring value types: score events, ops, configuration, snapshots,
//! and the membership-diff result.

use std::collections::HashMap;

/// Something a member did that moves their peer score. Each variant maps to
/// one delta in the score table (see [`default_score_deltas`]);
///
/// The first two groups come from the ECP consensus path: a member is
/// accused of a violation and the group votes. On a YES the accused takes a
/// violation-specific penalty and the accuser takes a flat reward; a NO flips
/// it onto the accuser.
///
/// The last group is observed locally at commit selection, with no vote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScoreEvent {
    // ── Target of an accepted ECP (the violation it's penalized for) ──
    /// Committed proposals that don't match the voted-on set.
    BrokenCommit,
    /// MLS proposal payload was malformed, or didn't match the voted action.
    BrokenMlsProposal,
    /// Failed to commit approved work within the inactivity window.
    CensorshipInactivity,

    // ── Creator of an ECP (rewarded if it passes, penalized if it doesn't) ──
    /// An emergency proposal this member raised was accepted.
    EmergencyYesCreator,
    /// An emergency proposal this member raised was rejected (false accusation).
    EmergencyNoCreator,

    // ── Observed locally at commit selection, no vote ──
    /// Committed a valid batch.
    SuccessfulCommit,
    /// Lost a commit race with the same proposals but different MLS entropy —
    /// honest participation (RFC: MUST NOT be misbehavior).
    HonestCommitAttempt,
    /// Competing commit with a different proposal set than the selected one
    /// (RFC: MUST be misbehavior).
    MisbehavingCommit,
}

/// A [`ScoreEvent`] paired with the member it applies to. The scoring
/// helpers produce these; [`crate::PeerScoringService::apply_op`] applies them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreOp {
    pub member_id: Vec<u8>,
    pub event: ScoreEvent,
}

/// RFC §Peer Scoring default delta for each [`ScoreEvent`]. Values are
/// placeholders pending an empirical tuning pass (see `docs/ROADMAP.md`).
/// Integrators that want different deltas pass their own map to
/// [`crate::PeerScoringService::new`].
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
    /// `ViolationType::SCORE_BELOW_THRESHOLD` ECP removal (RFC §Peer Scoring).
    pub threshold: i64,
}

impl Default for ScoringConfig {
    /// RFC §Peer Scoring defaults.
    fn default() -> Self {
        Self {
            default_score: DEFAULT_PEER_SCORE,
            threshold: DEFAULT_THRESHOLD_PEER_SCORE,
        }
    }
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
