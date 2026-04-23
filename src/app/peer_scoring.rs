//! [`PeerScoringService`] plus default in-memory backends. One instance
//! per [`User`](super::User); scores are keyed by `(group_id, member_id)`
//! via a [`PeerScoreStorage`] and mapped from events via a [`ScoringProvider`].

use std::collections::HashMap;

use crate::core::{PeerScoreStorage, ScoreEvent, ScoringConfig, ScoringProvider};

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
}

impl ScoringProvider for FixedScoringProvider {
    fn score_delta(&self, event: ScoreEvent) -> i64 {
        self.deltas.get(&event).copied().unwrap_or(0)
    }
}

// ── Service ─────────────────────────────────────────────────────────

/// Per-member score tracker, parameterised over a [`PeerScoreStorage`] and
/// a [`ScoringProvider`]. Detects members at/below `removal_threshold`.
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

    /// Apply `event` to a tracked member; returns the new score, or `None`
    /// if the member isn't tracked.
    pub fn apply_event(
        &mut self,
        group_id: &str,
        member_id: &[u8],
        event: ScoreEvent,
    ) -> Option<i64> {
        let current = self.storage.get(group_id, member_id)?;
        let delta = self.provider.score_delta(event);
        let new_score = current.saturating_add(delta);
        self.storage.set(group_id, member_id, new_score);
        Some(new_score)
    }

    pub fn score_for(&self, group_id: &str, member_id: &[u8]) -> Option<i64> {
        self.storage.get(group_id, member_id)
    }

    /// Force-set a score; used when applying a `GroupSync` from the steward.
    pub fn set_score(&mut self, group_id: &str, member_id: &[u8], score: i64) {
        self.storage.set(group_id, member_id, score);
    }

    pub fn members_below_threshold(&self, group_id: &str) -> Vec<Vec<u8>> {
        self.storage
            .all_scores(group_id)
            .into_iter()
            .filter(|(_, score)| *score <= self.config.removal_threshold)
            .map(|(id, _)| id)
            .collect()
    }

    pub fn is_below_threshold(&self, group_id: &str, member_id: &[u8]) -> bool {
        self.storage
            .get(group_id, member_id)
            .is_some_and(|s| s <= self.config.removal_threshold)
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

    fn default_config() -> ScoringConfig {
        ScoringConfig {
            default_score: 100,
            removal_threshold: 0,
        }
    }

    fn default_deltas() -> HashMap<ScoreEvent, i64> {
        HashMap::from([
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::BrokenMlsProposal, -30),
            (ScoreEvent::CensorshipInactivity, -40),
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::EmergencyNoCreator, -50),
            (ScoreEvent::SuccessfulCommit, 10),
            (ScoreEvent::NonFinalizedProposalCommit, -30),
        ])
    }

    fn make_service() -> PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider> {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::new(default_deltas()),
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

    #[test]
    fn test_apply_event_decreases_score() {
        let mut svc = make_service();
        let member = b"alice";
        svc.add_member(GROUP, member);

        let new_score = svc.apply_event(GROUP, member, ScoreEvent::EmergencyNoCreator);

        assert_eq!(new_score, Some(50));
        assert_eq!(svc.score_for(GROUP, member), Some(50));
    }

    #[test]
    fn test_apply_event_increases_score() {
        let mut svc = make_service();
        let member = b"alice";
        svc.add_member(GROUP, member);

        let new_score = svc.apply_event(GROUP, member, ScoreEvent::SuccessfulCommit);

        assert_eq!(new_score, Some(110));
    }

    #[test]
    fn test_apply_event_unknown_member_returns_none() {
        let mut svc = make_service();

        let result = svc.apply_event(GROUP, b"unknown", ScoreEvent::EmergencyNoCreator);

        assert_eq!(result, None);
    }

    #[test]
    fn test_multiple_events_accumulate() {
        let mut svc = make_service();
        let member = b"alice";
        svc.add_member(GROUP, member);

        svc.apply_event(GROUP, member, ScoreEvent::EmergencyNoCreator);
        svc.apply_event(GROUP, member, ScoreEvent::NonFinalizedProposalCommit);
        svc.apply_event(GROUP, member, ScoreEvent::SuccessfulCommit);

        assert_eq!(svc.score_for(GROUP, member), Some(30));
    }

    #[test]
    fn test_members_below_threshold() {
        let mut svc = make_service();
        svc.add_member(GROUP, b"alice");
        svc.add_member(GROUP, b"bob");
        svc.add_member(GROUP, b"charlie");

        svc.apply_event(GROUP, b"alice", ScoreEvent::EmergencyNoCreator);
        svc.apply_event(GROUP, b"alice", ScoreEvent::BrokenCommit);

        svc.apply_event(GROUP, b"charlie", ScoreEvent::EmergencyNoCreator);
        svc.apply_event(GROUP, b"charlie", ScoreEvent::EmergencyNoCreator);

        let below = svc.members_below_threshold(GROUP);
        assert_eq!(below.len(), 2);
        assert!(below.contains(&b"alice".to_vec()));
        assert!(below.contains(&b"charlie".to_vec()));
        assert!(!below.contains(&b"bob".to_vec()));
    }

    #[test]
    fn test_is_below_threshold() {
        let mut svc = make_service();
        svc.add_member(GROUP, b"alice");

        assert!(!svc.is_below_threshold(GROUP, b"alice"));

        svc.apply_event(GROUP, b"alice", ScoreEvent::EmergencyNoCreator);
        svc.apply_event(GROUP, b"alice", ScoreEvent::BrokenCommit);

        assert!(svc.is_below_threshold(GROUP, b"alice"));
    }

    #[test]
    fn test_is_below_threshold_unknown_member() {
        let svc = make_service();

        assert!(!svc.is_below_threshold(GROUP, b"unknown"));
    }

    #[test]
    fn test_score_saturates_no_overflow() {
        let mut svc = PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            FixedScoringProvider::new(HashMap::from([(ScoreEvent::SuccessfulCommit, i64::MAX)])),
            ScoringConfig {
                default_score: i64::MAX,
                removal_threshold: 0,
            },
        );
        svc.add_member(GROUP, b"alice");

        let new_score = svc.apply_event(GROUP, b"alice", ScoreEvent::SuccessfulCommit);

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

        let new_score = svc.apply_event(GROUP, b"alice", ScoreEvent::SuccessfulCommit);

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
            svc1.apply_event(GROUP, member, *event);
            svc2.apply_event(GROUP, member, *event);
        }

        assert_eq!(
            svc1.score_for(GROUP, b"alice"),
            svc2.score_for(GROUP, b"alice")
        );
        assert_eq!(svc1.score_for(GROUP, b"bob"), svc2.score_for(GROUP, b"bob"));
        assert_eq!(
            svc1.members_below_threshold(GROUP).len(),
            svc2.members_below_threshold(GROUP).len()
        );
    }

    #[test]
    fn test_false_accusation_penalty() {
        let mut svc = make_service();
        svc.add_member(GROUP, b"accuser");
        svc.add_member(GROUP, b"target");

        svc.apply_event(GROUP, b"accuser", ScoreEvent::EmergencyNoCreator);

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

        svc.apply_event(group_a, member, ScoreEvent::EmergencyNoCreator);

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

        svc.apply_event(group_a, b"alice", ScoreEvent::EmergencyNoCreator);
        svc.apply_event(group_a, b"alice", ScoreEvent::BrokenCommit);
        svc.apply_event(group_b, b"bob", ScoreEvent::EmergencyNoCreator);
        svc.apply_event(group_b, b"bob", ScoreEvent::BrokenCommit);

        let below_a = svc.members_below_threshold(group_a);
        let below_b = svc.members_below_threshold(group_b);

        assert_eq!(below_a, vec![b"alice".to_vec()]);
        assert_eq!(below_b, vec![b"bob".to_vec()]);
    }
}
