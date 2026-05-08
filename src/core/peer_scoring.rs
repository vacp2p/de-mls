//! Peer-scoring vocabulary, traits, reference [`PeerScoringService`],
//! and pure score-derivation helpers. The service is storage- and
//! policy-agnostic; concrete backends (e.g.
//! [`crate::app::InMemoryPeerScoreStorage`]) and delta-table providers
//! (e.g. [`crate::app::FixedScoringProvider`]) live in the app layer.

use prost::Message;
use std::collections::HashSet;

use crate::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, ViolationEvidence, conversation_update_request::Payload,
};

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

/// A score operation produced by core logic. The app layer feeds these
/// into its scoring service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoreOp {
    pub member_id: Vec<u8>,
    pub event: ScoreEvent,
}

// ── Scoring configuration ───────────────────────────────────────────

/// Maps each [`ScoreEvent`] to a signed score delta (positive = reward).
/// The app layer ships a fixed-table default impl.
pub trait ScoringProvider {
    fn score_delta(&self, event: ScoreEvent) -> i64;
}

#[derive(Debug, Clone)]
pub struct ScoringConfig {
    /// Score assigned to newly added members.
    pub default_score: i64,
    /// At or below this score, a member is eligible for
    /// `SCORE_BELOW_THRESHOLD` ECP removal (RFC §Peer Scoring).
    pub threshold: i64,
}

/// Default removal threshold (RFC §Peer Scoring `threshold_peer_score`).
pub const DEFAULT_THRESHOLD_PEER_SCORE: i64 = 0;

// ── Plug-in events ──────────────────────────────────────────────────

/// Event emitted by a [`PeerScoringPlugin`] when an apply call moves a
/// member across the configured threshold. Plug-ins return events from
/// every mutating call; the coordinator drains them at known safe
/// points and turns them into protocol actions (e.g. submitting
/// `SCORE_BELOW_THRESHOLD`). The `score` field carries the post-apply
/// value at the time of the cross.
///
/// "Untracked → tracked" via [`PeerScoringPlugin::add_member`] or a
/// [`PeerScoringPlugin::apply_snapshot`] entry counts as crossing from
/// above for cross-detection — a fresh entry that lands at-or-below
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

// ── ConversationSync snapshot ──────────────────────────────────────────────

/// Sparse snapshot of per-member scores for joiner bootstrap. Carries
/// only members whose score has diverged from `default_score`; the
/// recipient initialises every member at default via membership sync
/// before applying the snapshot, so missing entries imply default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScoreSnapshot {
    /// Members whose score `!= default_score`. Absolute scores, not deltas.
    pub diverged: Vec<(Vec<u8>, i64)>,
}

/// Per-member score persistence for a single group. One storage
/// instance per group; the app layer ships an in-memory default impl.
pub trait PeerScoreStorage {
    fn get(&self, member_id: &[u8]) -> Option<i64>;
    fn set(&mut self, member_id: &[u8], score: i64);
    fn remove(&mut self, member_id: &[u8]);
    fn all_scores(&self) -> Vec<(Vec<u8>, i64)>;
}

// ── Scoring-member diff ─────────────────────────────────────────────

/// Members to add to and remove from a scoring table to bring it into
/// sync with the current MLS membership.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScoringMemberDiff {
    pub to_add: Vec<Vec<u8>>,
    pub to_remove: Vec<Vec<u8>>,
}

/// Pure diff between a scoring table snapshot and an MLS member roster.
/// Caller applies the diff to its own [`PeerScoringPlugin`].
pub fn scoring_member_diff(scored: &[Vec<u8>], mls_members: &[Vec<u8>]) -> ScoringMemberDiff {
    let scored_set: HashSet<&[u8]> = scored.iter().map(Vec::as_slice).collect();
    let mls_set: HashSet<&[u8]> = mls_members.iter().map(Vec::as_slice).collect();

    let to_add = mls_members
        .iter()
        .filter(|m| !scored_set.contains(m.as_slice()))
        .cloned()
        .collect();
    let to_remove = scored
        .iter()
        .filter(|m| !mls_set.contains(m.as_slice()))
        .cloned()
        .collect();
    ScoringMemberDiff { to_add, to_remove }
}

// ── ECP score derivation ────────────────────────────────────────────

/// Score ops to apply when an emergency proposal resolves. Returns an
/// empty vector when the payload isn't an ECP or has no evidence.
///
/// - accepted target-bearing emergency → target penalty + creator reward.
/// - accepted `SCORE_BELOW_THRESHOLD` or `DEADLOCK` → creator reward only.
/// - rejected emergency → creator penalty.
pub fn emergency_score_ops(payload: &[u8], approved: bool) -> Vec<ScoreOp> {
    let Ok(req) = ConversationUpdateRequest::decode(payload) else {
        return Vec::new();
    };
    let Some(Payload::EmergencyCriteria(ec)) = req.payload else {
        return Vec::new();
    };
    let Some(evidence) = ec.evidence else {
        return Vec::new();
    };

    if approved {
        let mut ops = vec![creator_reward(&evidence)];
        if let Some(target_op) = evidence.target_score_op() {
            ops.push(target_op);
        }
        ops
    } else {
        vec![creator_penalty(&evidence)]
    }
}

fn creator_reward(ev: &ViolationEvidence) -> ScoreOp {
    ScoreOp {
        member_id: ev.creator_member_id.clone(),
        event: ScoreEvent::EmergencyYesCreator,
    }
}

fn creator_penalty(ev: &ViolationEvidence) -> ScoreOp {
    ScoreOp {
        member_id: ev.creator_member_id.clone(),
        event: ScoreEvent::EmergencyNoCreator,
    }
}

// ── Plug-in trait ───────────────────────────────────────────────────

/// Per-group peer-scoring plug-in. Mutating methods return any
/// [`PeerScoringEvent`]s the call produced; the coordinator drains them
/// at safe points and turns threshold crossings into protocol actions.
/// The plug-in itself owns no I/O — storage backends and event
/// publishing are caller concerns.
pub trait PeerScoringPlugin: Send + Sync + 'static {
    /// Start tracking a member at the plug-in's default score.
    ///
    /// MUST be called at most once per member: a duplicate call resets
    /// the score to default and discards any accumulated state. Callers
    /// guard with [`Self::score_for`] or a roster diff (see
    /// [`scoring_member_diff`]).
    ///
    /// Emits [`PeerScoringEvent::ThresholdCrossedDown`] iff the default
    /// score lands at-or-below threshold (unusual config); otherwise no
    /// event.
    #[must_use]
    fn add_member(&mut self, member_id: &[u8]) -> Vec<PeerScoringEvent>;

    /// Stop tracking a member. Emits no events — removal is a membership
    /// change, not a score event. Idempotent: a no-op on an untracked
    /// member.
    fn remove_member(&mut self, member_id: &[u8]);

    /// Apply a single [`ScoreOp`]. Untracked members are no-ops (no
    /// event, no auto-tracking — coordinators must call [`Self::add_member`]
    /// before applying ops). Returns events only when the op causes a
    /// threshold cross; repeated ops on a member already at-or-below
    /// threshold do not re-emit.
    #[must_use]
    fn apply_op(&mut self, op: &ScoreOp) -> Vec<PeerScoringEvent>;

    /// Apply a batch of [`ScoreOp`]s. Returns the concatenated events
    /// for every op in input order. A member crossing threshold twice
    /// in one batch (down then up, say) yields two events in that order.
    #[must_use]
    fn apply_ops(&mut self, ops: &[ScoreOp]) -> Vec<PeerScoringEvent>;

    /// Apply a [`ScoreSnapshot`] (ConversationSync receive). Each entry is an
    /// absolute-score replacement that auto-tracks members not already
    /// known to the plug-in (treating "untracked → tracked" as crossing
    /// from above for cross-detection — see [`PeerScoringEvent`]). This
    /// is the one mutator that diverges from [`Self::apply_op`]'s
    /// no-track-on-unknown behaviour: snapshots are bootstrap payloads
    /// and arrive before membership sync in some edge cases.
    ///
    /// Events fire only when the entry's prior state was on the
    /// opposite side of threshold from the new score; re-applying an
    /// unchanged snapshot emits nothing.
    #[must_use]
    fn apply_snapshot(&mut self, snapshot: &ScoreSnapshot) -> Vec<PeerScoringEvent>;

    /// Sparse snapshot of non-default scores for ConversationSync send.
    fn snapshot(&self) -> ScoreSnapshot;

    fn score_for(&self, member_id: &[u8]) -> Option<i64>;
    fn members_below_threshold(&self) -> Vec<Vec<u8>>;
    fn all_members_with_scores(&self) -> Vec<(Vec<u8>, i64)>;

    /// Current removal threshold. Coordinator reads this when building
    /// `ConversationSync` so joiners adopt the same value.
    fn threshold(&self) -> i64;

    /// Update the threshold in place. Emits NO events — even though a
    /// tightened threshold can re-classify members from above to
    /// at-or-below, the plug-in does not retroactively scan and emit.
    /// Event-driven coordinators MUST re-scan
    /// [`Self::members_below_threshold`] after a `set_threshold` call,
    /// or arrange for a subsequent apply that surfaces the affected
    /// members through normal cross-detection.
    fn set_threshold(&mut self, threshold: i64);
}

/// Compute the [`PeerScoringEvent`] for a transition from `prior` to
/// `new_score`. `prior == None` (untracked) is treated as "above
/// threshold" so a fresh entry landing at-or-below threshold emits a
/// downward cross. Returns `None` when no cross occurred.
fn cross_event(
    member_id: &[u8],
    prior: Option<i64>,
    new_score: i64,
    threshold: i64,
) -> Option<PeerScoringEvent> {
    let was_above = prior.is_none_or(|p| p > threshold);
    let now_below = new_score <= threshold;
    if was_above && now_below {
        Some(PeerScoringEvent::ThresholdCrossedDown {
            member_id: member_id.to_vec(),
            score: new_score,
        })
    } else if !was_above && new_score > threshold {
        Some(PeerScoringEvent::ThresholdCrossedUp {
            member_id: member_id.to_vec(),
            score: new_score,
        })
    } else {
        None
    }
}

// ── Reference scoring service ───────────────────────────────────────

/// Per-group, per-member score tracker. Reference [`PeerScoringPlugin`]
/// implementation. One instance per group; threshold travels with
/// [`ScoringConfig`]. Storage is abstracted via [`PeerScoreStorage`] so
/// app-layer backends (in-memory, on-disk, …) plug in without touching
/// this protocol logic.
pub struct PeerScoringService<S: PeerScoreStorage, P: ScoringProvider> {
    storage: S,
    provider: P,
    config: ScoringConfig,
}

impl<S: PeerScoreStorage, P: ScoringProvider> PeerScoringService<S, P> {
    pub fn new(storage: S, provider: P, config: ScoringConfig) -> Self {
        Self {
            storage,
            provider,
            config,
        }
    }
}

impl<S: PeerScoreStorage + Send + Sync + 'static, P: ScoringProvider + Send + Sync + 'static>
    PeerScoringPlugin for PeerScoringService<S, P>
{
    fn add_member(&mut self, member_id: &[u8]) -> Vec<PeerScoringEvent> {
        let default = self.config.default_score;
        self.storage.set(member_id, default);
        // "Untracked → tracked" treated as "above → new state" for
        // cross-detection purposes, so an unusual config with
        // `default_score <= threshold` still surfaces the new member as
        // a downward cross. The standard config (default 100, threshold
        // 0) silently produces no event.
        if default <= self.config.threshold {
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: member_id.to_vec(),
                score: default,
            }]
        } else {
            Vec::new()
        }
    }

    fn remove_member(&mut self, member_id: &[u8]) {
        self.storage.remove(member_id);
    }

    fn apply_op(&mut self, op: &ScoreOp) -> Vec<PeerScoringEvent> {
        let Some(current) = self.storage.get(&op.member_id) else {
            return Vec::new();
        };
        let delta = self.provider.score_delta(op.event);
        let new_score = current.saturating_add(delta);
        self.storage.set(&op.member_id, new_score);
        cross_event(
            &op.member_id,
            Some(current),
            new_score,
            self.config.threshold,
        )
        .into_iter()
        .collect()
    }

    fn apply_ops(&mut self, ops: &[ScoreOp]) -> Vec<PeerScoringEvent> {
        let mut events = Vec::new();
        for op in ops {
            events.extend(self.apply_op(op));
        }
        events
    }

    fn apply_snapshot(&mut self, snapshot: &ScoreSnapshot) -> Vec<PeerScoringEvent> {
        let threshold = self.config.threshold;
        let mut events = Vec::new();
        for (member_id, new_score) in &snapshot.diverged {
            let prior = self.storage.get(member_id);
            self.storage.set(member_id, *new_score);
            if let Some(ev) = cross_event(member_id, prior, *new_score, threshold) {
                events.push(ev);
            }
        }
        events
    }

    fn snapshot(&self) -> ScoreSnapshot {
        let default = self.config.default_score;
        let diverged = self
            .storage
            .all_scores()
            .into_iter()
            .filter(|(_, score)| *score != default)
            .collect();
        ScoreSnapshot { diverged }
    }

    fn score_for(&self, member_id: &[u8]) -> Option<i64> {
        self.storage.get(member_id)
    }

    fn members_below_threshold(&self) -> Vec<Vec<u8>> {
        let threshold = self.config.threshold;
        self.storage
            .all_scores()
            .into_iter()
            .filter(|(_, score)| *score <= threshold)
            .map(|(id, _)| id)
            .collect()
    }

    fn all_members_with_scores(&self) -> Vec<(Vec<u8>, i64)> {
        self.storage.all_scores()
    }

    fn threshold(&self) -> i64 {
        self.config.threshold
    }

    fn set_threshold(&mut self, threshold: i64) {
        self.config.threshold = threshold;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::protos::de_mls::messages::v1::{EmergencyCriteriaProposal, ViolationType};

    // ── Test scaffolding ────────────────────────────────────────────

    /// Minimal in-memory storage for service tests. Production storage
    /// lives in [`crate::app::InMemoryPeerScoreStorage`].
    #[derive(Default)]
    struct TestStorage(HashMap<Vec<u8>, i64>);

    impl PeerScoreStorage for TestStorage {
        fn get(&self, member_id: &[u8]) -> Option<i64> {
            self.0.get(member_id).copied()
        }
        fn set(&mut self, member_id: &[u8], score: i64) {
            self.0.insert(member_id.to_vec(), score);
        }
        fn remove(&mut self, member_id: &[u8]) {
            self.0.remove(member_id);
        }
        fn all_scores(&self) -> Vec<(Vec<u8>, i64)> {
            self.0.iter().map(|(k, v)| (k.clone(), *v)).collect()
        }
    }

    /// HashMap-backed [`ScoringProvider`] with caller-supplied deltas.
    /// Production deltas live in [`crate::app::FixedScoringProvider`].
    struct TestProvider(HashMap<ScoreEvent, i64>);

    impl ScoringProvider for TestProvider {
        fn score_delta(&self, event: ScoreEvent) -> i64 {
            self.0.get(&event).copied().unwrap_or(0)
        }
    }

    fn make_service() -> PeerScoringService<TestStorage, TestProvider> {
        let deltas = HashMap::from([
            (ScoreEvent::EmergencyNoCreator, -50),
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::SuccessfulCommit, 10),
            (ScoreEvent::MisbehavingCommit, -30),
        ]);
        PeerScoringService::new(
            TestStorage::default(),
            TestProvider(deltas),
            ScoringConfig {
                default_score: 100,
                threshold: 0,
            },
        )
    }

    // ── Service tests ────────────────────────────────────────────────

    #[test]
    fn add_member_gets_default_score() {
        let mut svc = make_service();
        let events = svc.add_member(b"alice");
        assert!(events.is_empty(), "default 100 > threshold 0, no cross");
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    #[test]
    fn add_member_with_default_below_threshold_emits_down_event() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            TestProvider(HashMap::new()),
            ScoringConfig {
                default_score: -10,
                threshold: 0,
            },
        );
        let events = svc.add_member(b"alice");
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: b"alice".to_vec(),
                score: -10,
            }]
        );
    }

    #[test]
    fn unknown_member_returns_none() {
        let svc = make_service();
        assert_eq!(svc.score_for(b"unknown"), None);
    }

    #[test]
    fn remove_member_clears_score() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        svc.remove_member(b"alice");
        assert_eq!(svc.score_for(b"alice"), None);
    }

    #[test]
    fn apply_event_decreases_score() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(events.is_empty(), "100 → 50 stays above threshold 0");
        assert_eq!(svc.score_for(b"alice"), Some(50));
    }

    #[test]
    fn apply_event_unknown_member_returns_no_events() {
        let mut svc = make_service();
        let events = svc.apply_op(&ScoreOp {
            member_id: b"unknown".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(events.is_empty());
    }

    #[test]
    fn multiple_events_accumulate() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        for event in [
            ScoreEvent::EmergencyNoCreator,
            ScoreEvent::MisbehavingCommit,
            ScoreEvent::SuccessfulCommit,
        ] {
            let _ = svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event,
            });
        }
        assert_eq!(svc.score_for(b"alice"), Some(30));
    }

    #[test]
    fn apply_op_emits_threshold_crossed_up_on_recovery() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        // 100 → 50 → 0 (down), 0 → 20 (up via EmergencyYesCreator).
        for _ in 0..2 {
            let _ = svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            });
        }
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyYesCreator,
        });
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedUp {
                member_id: b"alice".to_vec(),
                score: 20,
            }]
        );
    }

    #[test]
    fn threshold_cross_down_emits_event() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");

        // 100 → 50, still above threshold 0.
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert!(events.is_empty(), "above threshold, no event");

        // 50 → 0, crosses to at-or-below threshold.
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::EmergencyNoCreator,
        });
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: b"alice".to_vec(),
                score: 0,
            }]
        );

        // 0 → -50, already below — no further event.
        let events = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::BrokenCommit,
        });
        assert!(events.is_empty(), "already below threshold, no event");
    }

    #[test]
    fn apply_ops_concatenates_events_in_order() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        // Drop alice across threshold (-50 + -50 = 0 ≤ 0) and bob too in
        // the same batch.
        let ops = vec![
            ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
            ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
            ScoreOp {
                member_id: b"bob".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
            ScoreOp {
                member_id: b"bob".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
        ];
        let events = svc.apply_ops(&ops);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            PeerScoringEvent::ThresholdCrossedDown { ref member_id, .. } if member_id == b"alice"
        ));
        assert!(matches!(
            events[1],
            PeerScoringEvent::ThresholdCrossedDown { ref member_id, .. } if member_id == b"bob"
        ));
    }

    #[test]
    fn snapshot_includes_only_diverged_scores() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        let _ = svc.add_member(b"charlie");
        let _ = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });
        let snap = svc.snapshot();
        let ids: Vec<&[u8]> = snap.diverged.iter().map(|(id, _)| id.as_slice()).collect();
        assert_eq!(ids, vec![b"alice".as_slice()]);
        assert_eq!(snap.diverged[0].1, 110);
    }

    #[test]
    fn apply_snapshot_emits_event_only_on_actual_cross() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        let snap = ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10), (b"bob".to_vec(), 50)],
        };
        let events = svc.apply_snapshot(&snap);
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: b"alice".to_vec(),
                score: -10,
            }]
        );
        assert_eq!(svc.score_for(b"alice"), Some(-10));
        assert_eq!(svc.score_for(b"bob"), Some(50));
    }

    #[test]
    fn apply_snapshot_idempotent_on_repeat() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let snap = ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        };
        let first = svc.apply_snapshot(&snap);
        assert_eq!(first.len(), 1, "first apply emits the cross");
        let second = svc.apply_snapshot(&snap);
        assert!(
            second.is_empty(),
            "second apply on unchanged state emits nothing"
        );
    }

    #[test]
    fn apply_snapshot_emits_threshold_crossed_up_on_recovery() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        // First push alice below threshold.
        let _ = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        });
        // Now snapshot her back above.
        let events = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), 50)],
        });
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedUp {
                member_id: b"alice".to_vec(),
                score: 50,
            }]
        );
    }

    #[test]
    fn apply_snapshot_for_untracked_below_threshold_emits_down() {
        // Untracked → tracked (below threshold) treated as a downward
        // cross from "above-by-default."
        let mut svc = make_service();
        let events = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"newcomer".to_vec(), -10)],
        });
        assert_eq!(
            events,
            vec![PeerScoringEvent::ThresholdCrossedDown {
                member_id: b"newcomer".to_vec(),
                score: -10,
            }]
        );
    }

    #[test]
    fn members_below_threshold_filters_correctly() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        let _ = svc.add_member(b"bob");
        let _ = svc.add_member(b"charlie");
        for event in [ScoreEvent::EmergencyNoCreator, ScoreEvent::BrokenCommit] {
            let _ = svc.apply_op(&ScoreOp {
                member_id: b"alice".to_vec(),
                event,
            });
        }
        for _ in 0..2 {
            let _ = svc.apply_op(&ScoreOp {
                member_id: b"charlie".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            });
        }
        let below = svc.members_below_threshold();
        assert!(below.contains(&b"alice".to_vec()));
        assert!(below.contains(&b"charlie".to_vec()));
        assert!(!below.contains(&b"bob".to_vec()));
    }

    #[test]
    fn set_threshold_changes_below_threshold_set() {
        let mut svc = make_service();
        let _ = svc.add_member(b"alice");
        // Apply via snapshot to set an absolute score without going
        // through the delta provider.
        let _ = svc.apply_snapshot(&ScoreSnapshot {
            diverged: vec![(b"alice".to_vec(), -10)],
        });

        svc.set_threshold(-50);
        assert!(!svc.members_below_threshold().contains(&b"alice".to_vec()));

        svc.set_threshold(-5);
        assert!(svc.members_below_threshold().contains(&b"alice".to_vec()));
    }

    #[test]
    fn score_saturates_no_overflow() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            TestProvider(HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)])),
            ScoringConfig {
                default_score: i64::MAX,
                threshold: 0,
            },
        );
        let _ = svc.add_member(b"alice");
        let _ = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });
        assert_eq!(svc.score_for(b"alice"), Some(i64::MAX));
    }

    #[test]
    fn unknown_event_yields_zero_delta() {
        let mut svc = PeerScoringService::new(
            TestStorage::default(),
            TestProvider(HashMap::from([(ScoreEvent::EmergencyNoCreator, -50)])),
            ScoringConfig {
                default_score: 100,
                threshold: 0,
            },
        );
        let _ = svc.add_member(b"alice");
        let _ = svc.apply_op(&ScoreOp {
            member_id: b"alice".to_vec(),
            event: ScoreEvent::SuccessfulCommit,
        });
        assert_eq!(svc.score_for(b"alice"), Some(100));
    }

    // ── ECP score derivation tests ──────────────────────────────────

    fn ecp_payload(violation_type: i32, target: Vec<u8>, creator: Vec<u8>) -> Vec<u8> {
        let evidence = ViolationEvidence {
            violation_type,
            target_member_id: target,
            evidence_payload: Vec::new(),
            epoch: 0,
            creator_member_id: creator,
        };
        let req = ConversationUpdateRequest {
            payload: Some(Payload::EmergencyCriteria(EmergencyCriteriaProposal {
                evidence: Some(evidence),
            })),
        };
        req.encode_to_vec()
    }

    /// Approved + target-mappable violation → creator reward + target penalty.
    #[test]
    fn approved_broken_commit_emits_reward_and_target_penalty() {
        let payload = ecp_payload(ViolationType::BrokenCommit as i32, vec![0xAA], vec![0xBB]);
        let ops = emergency_score_ops(&payload, true);
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
        assert_eq!(ops[0].member_id, vec![0xBB]);
        assert_eq!(ops[1].event, ScoreEvent::BrokenCommit);
        assert_eq!(ops[1].member_id, vec![0xAA]);
    }

    /// Approved + non-target violation (`Deadlock`) → creator reward only.
    #[test]
    fn approved_deadlock_emits_reward_only() {
        let payload = ecp_payload(ViolationType::Deadlock as i32, Vec::new(), vec![0xBB]);
        let ops = emergency_score_ops(&payload, true);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Approved `ScoreBelowThreshold` is the trigger for removal — no
    /// target-side score op (the target gets removed instead).
    #[test]
    fn approved_score_below_threshold_emits_reward_only() {
        let payload = ecp_payload(
            ViolationType::ScoreBelowThreshold as i32,
            vec![0xAA],
            vec![0xBB],
        );
        let ops = emergency_score_ops(&payload, true);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Wire-malformed `Unspecified` violation type — emit creator reward,
    /// drop the malformed target op silently.
    #[test]
    fn approved_unspecified_emits_reward_only() {
        let payload = ecp_payload(0, vec![0xAA], vec![0xBB]);
        let ops = emergency_score_ops(&payload, true);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].event, ScoreEvent::EmergencyYesCreator);
    }

    /// Rejected → creator penalty regardless of violation type.
    #[test]
    fn rejected_emits_creator_penalty() {
        for vt in [
            ViolationType::BrokenCommit,
            ViolationType::Deadlock,
            ViolationType::ScoreBelowThreshold,
        ] {
            let payload = ecp_payload(vt as i32, vec![0xAA], vec![0xBB]);
            let ops = emergency_score_ops(&payload, false);
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].event, ScoreEvent::EmergencyNoCreator);
            assert_eq!(ops[0].member_id, vec![0xBB]);
        }
    }
}
