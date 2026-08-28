//! Peer-scoring value types: score events, ops, configuration, snapshots,
//! reported changes, and the membership-diff result.

use std::collections::HashMap;

use crate::ConversationError;

/// An action by a group member that affects their peer score.
/// Each kind changes the score by a fixed amount (see [`default_score_deltas`]).
///
/// - The first two groups are for votes during ECP consensus:
///   - If a violation is confirmed, the accused loses points and the accuser gets a reward.
///   - If not, the accuser is penalized instead.
/// - The last group is recorded when selecting commits, without voting.
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
    /// Wrongdoing caught after staging that isn't a broken commit or proposal —
    /// e.g. a committer who isn't on the steward list. Milder penalty.
    MisbehavingCommit,
}

impl ScoreEvent {
    /// The default score change for this event (RFC §Peer Scoring). Negative
    /// for a penalty, positive for a reward.
    ///
    /// An exhaustive match, so adding a variant without a value is a compile
    /// error — no event can end up silently worth zero.
    pub const fn default_delta(self) -> i64 {
        match self {
            // ECP target penalties (violation-type-specific)
            Self::BrokenCommit => -50,
            Self::BrokenMlsProposal => -30,
            Self::CensorshipInactivity => -40,
            // ECP creator outcomes
            Self::EmergencyYesCreator => 20,
            Self::EmergencyNoCreator => -50,
            // Commit selection
            Self::SuccessfulCommit => 10,
            Self::HonestCommitAttempt => 5,
            // Milder than a broken commit/proposal: wrongdoing such as
            // committing while not on the steward list, caught after staging.
            Self::MisbehavingCommit => -10,
        }
    }
}

/// Links a [`ScoreEvent`] to the member it affects.
/// Scoring helpers create these, and [`crate::PeerScoringService::apply_ops`] applies them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreOp {
    pub member_id: Vec<u8>,
    pub event: ScoreEvent,
}

/// The whole default delta table. [`PeerScoringService`](crate::PeerScoringService)
/// starts from it and lays the caller's overrides on top, so an integrator only
/// needs this to see or tune the values.
pub fn default_score_deltas() -> HashMap<ScoreEvent, i64> {
    [
        ScoreEvent::BrokenCommit,
        ScoreEvent::BrokenMlsProposal,
        ScoreEvent::CensorshipInactivity,
        ScoreEvent::EmergencyYesCreator,
        ScoreEvent::EmergencyNoCreator,
        ScoreEvent::SuccessfulCommit,
        ScoreEvent::HonestCommitAttempt,
        ScoreEvent::MisbehavingCommit,
    ]
    .into_iter()
    .map(|event| (event, event.default_delta()))
    .collect()
}

/// RFC §Peer Scoring `default_peer_score`: starting score for a new member.
pub const DEFAULT_PEER_SCORE: i64 = 100;

/// RFC §Peer Scoring `threshold_peer_score`: default removal threshold.
pub const DEFAULT_THRESHOLD_PEER_SCORE: i64 = 0;

#[derive(Debug, Clone)]
pub struct ScoringConfig {
    /// The score every new member starts with. This is shared across the group—
    /// joiners use the group's value from [`ConversationSync`](crate::protos::de_mls::messages::v1::ConversationSync).
    pub default_score: i64,
    /// Members at or below this score can be count as malicious.
    /// The application may specify concrete action when they got event.
    pub threshold: i64,
}

impl ScoringConfig {
    /// Checks that the starting score setup makes sense:
    /// - default_score must be above zero
    /// - default_score must be bigger than threshold
    ///   Use this for local configs or when joining from a ConversationSync.
    pub fn validate(&self) -> Result<(), ConversationError> {
        if self.default_score <= 0 {
            return Err(ConversationError::InvalidConfig(format!(
                "default_peer_score must be positive, got {}",
                self.default_score
            )));
        }
        if self.default_score <= self.threshold {
            return Err(ConversationError::InvalidConfig(
                "default_peer_score must be greater than threshold".to_string(),
            ));
        }
        Ok(())
    }
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

/// Snapshot of member scores for bootstrap: only non-default scores are listed.
/// Missing entries mean the score is still at the group's default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScoreSnapshot {
    pub diverged: Vec<(Vec<u8>, i64)>,
}

/// A member whose score changed.
///
/// Shows just the final score after a batch of updates.
/// If a member's score doesn't actually change, nothing is reported.
///
/// The library just reports the change; it's up to app what to do with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreChange {
    pub member_id: Vec<u8>,
    /// Score before the update
    pub previous: i64,
    /// Score after the update
    pub score: i64,
}

/// Members to add to and remove from a scoring table to bring it into
/// sync with the current MLS membership.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScoringMemberDiff {
    pub to_add: Vec<Vec<u8>>,
    pub to_remove: Vec<Vec<u8>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A starting score is a positive quantity, whatever it is measured
    /// against — the same rule an incoming `ConversationSync` is held to.
    #[test]
    fn validate_accepts_any_positive_default() {
        for (default_score, threshold) in [(100, 0), (1, 0), (i64::MAX, -50)] {
            assert!(
                ScoringConfig {
                    default_score,
                    threshold,
                }
                .validate()
                .is_ok(),
                "default {default_score} against threshold {threshold} is the group's call"
            );
        }
    }

    /// Zero or negative is not a score to start from. Caught at construction
    /// rather than leaving a group whose sync every joiner refuses.
    #[test]
    fn validate_rejects_a_non_positive_default() {
        for default_score in [0, -1, i64::MIN] {
            assert!(
                ScoringConfig {
                    default_score,
                    threshold: 0,
                }
                .validate()
                .is_err(),
                "default {default_score} must be rejected"
            );
        }
    }

    /// Rejects config where default_score is not greater than threshold.
    #[test]
    fn validate_rejects_default_not_greater_than_threshold() {
        let cases = [(1, 1), (5, 10), (0, 0)];
        for (default_score, threshold) in cases {
            assert!(
                ScoringConfig {
                    default_score,
                    threshold,
                }
                .validate()
                .is_err(),
                "default {default_score} <= threshold {threshold} must be rejected"
            );
        }
    }

    #[test]
    fn rfc_defaults_validate() {
        assert!(ScoringConfig::default().validate().is_ok());
    }
}
