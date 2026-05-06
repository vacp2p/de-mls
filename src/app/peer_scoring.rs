//! App-layer scoring backends: an in-memory storage and a
//! HashMap-backed [`ScoringProvider`] with RFC-default deltas. Both
//! plug into the reference [`PeerScoringService`](crate::core::PeerScoringService)
//! in core.

use std::collections::HashMap;

use crate::core::{PeerScoreStorage, ScoreEvent, ScoringProvider};

// ── In-memory storage ───────────────────────────────────────────────

/// `HashMap`-backed [`PeerScoreStorage`] for tests and simple deployments.
/// Production integrators should supply a durable backend.
#[derive(Debug, Clone, Default)]
pub struct InMemoryPeerScoreStorage {
    scores: HashMap<Vec<u8>, i64>,
}

impl InMemoryPeerScoreStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PeerScoreStorage for InMemoryPeerScoreStorage {
    fn get(&self, member_id: &[u8]) -> Option<i64> {
        self.scores.get(member_id).copied()
    }

    fn set(&mut self, member_id: &[u8], score: i64) {
        self.scores.insert(member_id.to_vec(), score);
    }

    fn remove(&mut self, member_id: &[u8]) {
        self.scores.remove(member_id);
    }

    fn all_scores(&self) -> Vec<(Vec<u8>, i64)> {
        self.scores.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

// ── Default provider ───────────────────────────────────────────────

/// Constant [`ScoringProvider`] — events not in the map produce a delta of 0.
/// Default deltas via [`Self::default_deltas`] encode RFC §Peer Scoring
/// recommendations; integrators may swap in their own values.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_storage_round_trip() {
        let mut storage = InMemoryPeerScoreStorage::new();
        assert_eq!(storage.get(b"alice"), None);
        storage.set(b"alice", 42);
        assert_eq!(storage.get(b"alice"), Some(42));
        storage.set(b"bob", -3);
        let all = storage.all_scores();
        assert_eq!(all.len(), 2);
        storage.remove(b"alice");
        assert_eq!(storage.get(b"alice"), None);
        assert_eq!(storage.all_scores().len(), 1);
    }

    #[test]
    fn fixed_provider_returns_zero_for_unknown_event() {
        let p = FixedScoringProvider::new(HashMap::new());
        assert_eq!(p.score_delta(ScoreEvent::SuccessfulCommit), 0);
    }

    #[test]
    fn fixed_provider_returns_default_deltas() {
        let p = FixedScoringProvider::with_default_deltas();
        assert_eq!(p.score_delta(ScoreEvent::SuccessfulCommit), 10);
        assert_eq!(p.score_delta(ScoreEvent::BrokenCommit), -50);
        assert_eq!(p.score_delta(ScoreEvent::EmergencyYesCreator), 20);
    }
}
