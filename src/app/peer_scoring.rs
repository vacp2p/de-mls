//! [`PeerScoringService`] plus default in-memory backends. One instance
//! per [`User`](super::User); scores are keyed by `(group_id, member_id)`
//! via a [`PeerScoreStorage`] and mapped from events via a [`ScoringProvider`].

use std::collections::HashMap;

use crate::core::{PeerScoreStorage, ScoreEvent, ScoreOp, ScoringConfig, ScoringProvider};

// ── In-memory storage ───────────────────────────────────────────────

/// `HashMap`-backed [`PeerScoreStorage`] for tests and simple deployments.
/// Production integrators should supply a durable backend.
#[derive(Debug, Clone, Default)]
pub struct InMemoryPeerScoreStorage {
    scores: HashMap<String, HashMap<Vec<u8>, i64>>,
}

impl InMemoryPeerScoreStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PeerScoreStorage for InMemoryPeerScoreStorage {
    fn get(&self, group_id: &str, member_id: &[u8]) -> Option<i64> {
        self.scores
            .get(group_id)
            .and_then(|members| members.get(member_id).copied())
    }

    fn set(&mut self, group_id: &str, member_id: &[u8], score: i64) {
        self.scores
            .entry(group_id.to_string())
            .or_default()
            .insert(member_id.to_vec(), score);
    }

    fn remove(&mut self, group_id: &str, member_id: &[u8]) {
        if let Some(members) = self.scores.get_mut(group_id) {
            members.remove(member_id);
        }
    }

    fn all_scores(&self, group_id: &str) -> Vec<(Vec<u8>, i64)> {
        self.scores
            .get(group_id)
            .map(|members| members.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default()
    }

    fn remove_group(&mut self, group_id: &str) {
        self.scores.remove(group_id);
    }
}

// ── Default provider ───────────────────────────────────────────────

/// Constant [`ScoringProvider`] — events not in the map produce a delta of 0.
#[derive(Debug, Clone)]
pub struct FixedScoringProvider {
    deltas: HashMap<ScoreEvent, i64>,
}

impl FixedScoringProvider {
    pub fn new(deltas: HashMap<ScoreEvent, i64>) -> Self {
        Self { deltas }
    }

    /// Default delta for each [`ScoreEvent`]. Values are placeholders
    /// pending an empirical tuning pass (see `docs/ROADMAP.md`).
    /// `FreezeViolation` is reserved vocabulary with no producer yet and
    /// is deliberately omitted.
    pub fn default_deltas() -> HashMap<ScoreEvent, i64> {
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
            // Not yet wired
            (ScoreEvent::NonFinalizedProposalCommit, -30),
        ])
    }

    /// Convenience constructor: [`Self::new`] with [`Self::default_deltas`].
    pub fn with_default_deltas() -> Self {
        Self::new(Self::default_deltas())
    }
}

impl ScoringProvider for FixedScoringProvider {
    fn score_delta(&self, event: ScoreEvent) -> i64 {
        self.deltas.get(&event).copied().unwrap_or(0)
    }
}

// ── Service ─────────────────────────────────────────────────────────

/// Per-member score tracker. Threshold is supplied per-call by the caller
/// (held on `Group::threshold_peer_score`).
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

    /// Start tracking a member with the default score.
    pub fn add_member(&mut self, group_id: &str, member_id: &[u8]) {
        self.storage
            .set(group_id, member_id, self.config.default_score);
    }

    pub fn remove_member(&mut self, group_id: &str, member_id: &[u8]) {
        self.storage.remove(group_id, member_id);
    }

    /// Drop every score entry for `group_id`. Called on leave.
    pub fn remove_group(&mut self, group_id: &str) {
        self.storage.remove_group(group_id);
    }

    /// Apply a single [`ScoreOp`] and return the target's new score, or
    /// `None` if the target isn't tracked.
    pub fn apply_op(&mut self, group_id: &str, op: &ScoreOp) -> Option<i64> {
        let current = self.storage.get(group_id, &op.member_id)?;
        let delta = self.provider.score_delta(op.event);
        let new_score = current.saturating_add(delta);
        self.storage.set(group_id, &op.member_id, new_score);
        Some(new_score)
    }

    /// Apply a batch of [`ScoreOp`]s. Untracked targets are skipped
    /// silently (same semantics as [`Self::apply_op`]).
    pub fn apply_ops(&mut self, group_id: &str, ops: &[ScoreOp]) {
        for op in ops {
            self.apply_op(group_id, op);
        }
    }

    pub fn score_for(&self, group_id: &str, member_id: &[u8]) -> Option<i64> {
        self.storage.get(group_id, member_id)
    }

    /// Force-set a score; used when applying a `GroupSync` from the steward.
    pub fn set_score(&mut self, group_id: &str, member_id: &[u8], score: i64) {
        self.storage.set(group_id, member_id, score);
    }

    pub fn members_below_threshold(&self, group_id: &str, threshold: i64) -> Vec<Vec<u8>> {
        self.storage
            .all_scores(group_id)
            .into_iter()
            .filter(|(_, score)| *score <= threshold)
            .map(|(id, _)| id)
            .collect()
    }

    pub fn is_below_threshold(&self, group_id: &str, member_id: &[u8], threshold: i64) -> bool {
        self.storage
            .get(group_id, member_id)
            .is_some_and(|s| s <= threshold)
    }

    pub fn all_members_with_scores(&self, group_id: &str) -> Vec<(Vec<u8>, i64)> {
        self.storage.all_scores(group_id)
    }

    pub fn config(&self) -> &ScoringConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUP: &str = "test-group";
    const REMOVAL_THRESHOLD: i64 = 0;

    fn default_config() -> ScoringConfig {
        ScoringConfig { default_score: 100 }
    }

    fn make_service() -> PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider> {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::with_default_deltas(),
            default_config(),
        )
    }

    #[test]
    fn test_add_member_gets_default_score() {
        let mut svc = make_service();
        let member = b"alice";

        svc.add_member(GROUP, member);

        assert_eq!(svc.score_for(GROUP, member), Some(100));
    }

    #[test]
    fn test_unknown_member_returns_none() {
        let svc = make_service();

        assert_eq!(svc.score_for(GROUP, b"unknown"), None);
    }

    #[test]
    fn test_remove_member() {
        let mut svc = make_service();
        let member = b"alice";

        svc.add_member(GROUP, member);
        svc.remove_member(GROUP, member);

        assert_eq!(svc.score_for(GROUP, member), None);
    }

    /// `remove_group` drops every score in the target group while leaving
    /// other groups untouched. Idempotent on an unknown group_id.
    #[test]
    fn test_remove_group_clears_only_target_group() {
        let mut svc = make_service();
        let other_group = "other-group";

        svc.add_member(GROUP, b"alice");
        svc.add_member(GROUP, b"bob");
        svc.add_member(other_group, b"carol");

        svc.remove_group(GROUP);

        assert_eq!(svc.score_for(GROUP, b"alice"), None);
        assert_eq!(svc.score_for(GROUP, b"bob"), None);
        assert!(svc.all_members_with_scores(GROUP).is_empty());
        assert_eq!(
            svc.score_for(other_group, b"carol"),
            Some(default_config().default_score)
        );

        // Idempotent on a group that was never tracked.
        svc.remove_group("never-existed");
    }

    #[test]
    fn test_apply_event_decreases_score() {
        let mut svc = make_service();
        let member = b"alice";
        svc.add_member(GROUP, member);

        let new_score = svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: member.to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );

        assert_eq!(new_score, Some(50));
        assert_eq!(svc.score_for(GROUP, member), Some(50));
    }

    #[test]
    fn test_apply_event_increases_score() {
        let mut svc = make_service();
        let member = b"alice";
        svc.add_member(GROUP, member);

        let new_score = svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: member.to_vec(),
                event: ScoreEvent::SuccessfulCommit,
            },
        );

        assert_eq!(new_score, Some(110));
    }

    #[test]
    fn test_apply_event_unknown_member_returns_none() {
        let mut svc = make_service();

        let result = svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"unknown".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );

        assert_eq!(result, None);
    }

    #[test]
    fn test_multiple_events_accumulate() {
        let mut svc = make_service();
        let member = b"alice";
        svc.add_member(GROUP, member);

        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: member.to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );
        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: member.to_vec(),
                event: ScoreEvent::NonFinalizedProposalCommit,
            },
        );
        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: member.to_vec(),
                event: ScoreEvent::SuccessfulCommit,
            },
        );

        assert_eq!(svc.score_for(GROUP, member), Some(30));
    }

    #[test]
    fn test_members_below_threshold() {
        let mut svc = make_service();
        svc.add_member(GROUP, b"alice");
        svc.add_member(GROUP, b"bob");
        svc.add_member(GROUP, b"charlie");

        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );
        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
        );

        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"charlie".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );
        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"charlie".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );

        let below = svc.members_below_threshold(GROUP, REMOVAL_THRESHOLD);
        assert_eq!(below.len(), 2);
        assert!(below.contains(&b"alice".to_vec()));
        assert!(below.contains(&b"charlie".to_vec()));
        assert!(!below.contains(&b"bob".to_vec()));
    }

    #[test]
    fn test_is_below_threshold() {
        let mut svc = make_service();
        svc.add_member(GROUP, b"alice");

        assert!(!svc.is_below_threshold(GROUP, b"alice", REMOVAL_THRESHOLD));

        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );
        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
        );

        assert!(svc.is_below_threshold(GROUP, b"alice", REMOVAL_THRESHOLD));
    }

    #[test]
    fn test_is_below_threshold_unknown_member() {
        let svc = make_service();

        assert!(!svc.is_below_threshold(GROUP, b"unknown", REMOVAL_THRESHOLD));
    }

    #[test]
    fn test_score_saturates_no_overflow() {
        let mut svc = PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::new(HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)])),
            ScoringConfig {
                default_score: i64::MAX,
            },
        );
        svc.add_member(GROUP, b"alice");

        let new_score = svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::SuccessfulCommit,
            },
        );

        assert_eq!(new_score, Some(i64::MAX));
    }

    #[test]
    fn test_unknown_event_in_provider_returns_zero_delta() {
        let mut svc = PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::new(HashMap::from([(ScoreEvent::EmergencyNoCreator, -50)])),
            default_config(),
        );
        svc.add_member(GROUP, b"alice");

        let new_score = svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::SuccessfulCommit,
            },
        );

        assert_eq!(new_score, Some(100));
    }

    #[test]
    fn test_determinism_independent_instances() {
        let events = vec![
            (b"alice".as_slice(), ScoreEvent::EmergencyNoCreator),
            (b"alice".as_slice(), ScoreEvent::SuccessfulCommit),
            (b"bob".as_slice(), ScoreEvent::BrokenCommit),
            (b"bob".as_slice(), ScoreEvent::SuccessfulCommit),
        ];

        let mut svc1 = make_service();
        let mut svc2 = make_service();

        for svc in [&mut svc1, &mut svc2] {
            svc.add_member(GROUP, b"alice");
            svc.add_member(GROUP, b"bob");
        }

        for (member, event) in &events {
            svc1.apply_op(
                GROUP,
                &ScoreOp {
                    member_id: member.to_vec(),
                    event: *event,
                },
            );
            svc2.apply_op(
                GROUP,
                &ScoreOp {
                    member_id: member.to_vec(),
                    event: *event,
                },
            );
        }

        assert_eq!(
            svc1.score_for(GROUP, b"alice"),
            svc2.score_for(GROUP, b"alice")
        );
        assert_eq!(svc1.score_for(GROUP, b"bob"), svc2.score_for(GROUP, b"bob"));
        assert_eq!(
            svc1.members_below_threshold(GROUP, REMOVAL_THRESHOLD).len(),
            svc2.members_below_threshold(GROUP, REMOVAL_THRESHOLD).len()
        );
    }

    #[test]
    fn test_false_accusation_penalty() {
        let mut svc = make_service();
        svc.add_member(GROUP, b"accuser");
        svc.add_member(GROUP, b"target");

        svc.apply_op(
            GROUP,
            &ScoreOp {
                member_id: b"accuser".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );

        assert_eq!(svc.score_for(GROUP, b"accuser"), Some(50));
        assert_eq!(svc.score_for(GROUP, b"target"), Some(100));
    }

    #[test]
    fn test_scores_isolated_between_groups() {
        let mut svc = make_service();
        let group_a = "group-a";
        let group_b = "group-b";
        let member = b"alice";

        svc.add_member(group_a, member);
        svc.add_member(group_b, member);

        svc.apply_op(
            group_a,
            &ScoreOp {
                member_id: member.to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );

        assert_eq!(svc.score_for(group_a, member), Some(50));
        assert_eq!(svc.score_for(group_b, member), Some(100));
    }

    #[test]
    fn test_members_below_threshold_only_returns_group_members() {
        let mut svc = make_service();
        let group_a = "group-a";
        let group_b = "group-b";

        svc.add_member(group_a, b"alice");
        svc.add_member(group_b, b"bob");

        svc.apply_op(
            group_a,
            &ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );
        svc.apply_op(
            group_a,
            &ScoreOp {
                member_id: b"alice".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
        );
        svc.apply_op(
            group_b,
            &ScoreOp {
                member_id: b"bob".to_vec(),
                event: ScoreEvent::EmergencyNoCreator,
            },
        );
        svc.apply_op(
            group_b,
            &ScoreOp {
                member_id: b"bob".to_vec(),
                event: ScoreEvent::BrokenCommit,
            },
        );

        let below_a = svc.members_below_threshold(group_a, REMOVAL_THRESHOLD);
        let below_b = svc.members_below_threshold(group_b, REMOVAL_THRESHOLD);

        assert_eq!(below_a, vec![b"alice".to_vec()]);
        assert_eq!(below_b, vec![b"bob".to_vec()]);
    }
}
