//! Peer scoring service and default implementations.
//!
//! This module provides [`PeerScoringService`], a standalone service that tracks
//! per-member reputation scores scoped by group. It lives at the application layer
//! because it orchestrates core-defined traits ([`PeerScoreStorage`], [`ScoringProvider`])
//! and is designed to be instantiated once per [`User`](super::User), not per group.
//!
//! # Architecture
//!
//! ```text
//! ┌───────────────────────────────────────────────────────┐
//! │              PeerScoringService<S, P>                 │
//! │                  (one per User)                       │
//! │                                                       │
//! │  ┌─────────────────┐  ┌─────────────────────────────┐ │
//! │  │ ScoringProvider │  │ PeerScoreStorage            │ │
//! │  │ (event → delta) │  │ key: (group_id, member_id)  │ │
//! │  └─────────────────┘  └─────────────────────────────┘ │
//! └───────────────────────────────────────────────────────┘
//! ```
//!
//! # Default implementations
//!
//! - [`InMemoryPeerScoreStorage`] — `HashMap`-backed storage for testing/development
//! - [`FixedScoringProvider`]— static delta table

use std::collections::HashMap;

use crate::core::{PeerScoreStorage, ScoreEvent, ScoringConfig, ScoringProvider};

// ── In-memory storage ───────────────────────────────────────────────

/// In-memory score storage backed by a nested `HashMap`.
///
/// Scores are keyed by `(group_id, member_id)`. Suitable for testing and
/// development; production deployments should implement [`PeerScoreStorage`]
/// with a durable backend.
#[derive(Debug, Clone, Default)]
pub struct InMemoryPeerScoreStorage {
    /// group_id → (member_id → score)
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

/// Fixed score deltas backed by a `HashMap`.
///
/// Constructed once (typically at startup) and never changes. Events not
/// present in the map produce a delta of 0.
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

/// Standalone peer scoring service.
///
/// Tracks per-member scores scoped by group, applies events using a
/// [`ScoringProvider`], and detects members below the removal threshold.
/// Designed to be instantiated once per [`User`](super::User) with storage
/// keyed by `(group_id, member_id)`.
///
/// # Type parameters
///
/// - `S`: Storage backend (e.g. [`InMemoryPeerScoreStorage`])
/// - `P`: Scoring provider (e.g. [`FixedScoringProvider`])
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

    /// Register a new member in a group with the default score.
    pub fn add_member(&mut self, group_id: &str, member_id: &[u8]) {
        self.storage
            .set(group_id, member_id, self.config.default_score);
    }

    /// Remove a member from score tracking in a group.
    pub fn remove_member(&mut self, group_id: &str, member_id: &[u8]) {
        self.storage.remove(group_id, member_id);
    }

    /// Apply a score event to a member in a group.
    ///
    /// Returns the member's new score, or `None` if the member is not tracked.
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

    /// Query a member's current score in a group.
    pub fn score_for(&self, group_id: &str, member_id: &[u8]) -> Option<i64> {
        self.storage.get(group_id, member_id)
    }

    /// Set a member's score directly (used when receiving GroupSync from steward).
    pub fn set_score(&mut self, group_id: &str, member_id: &[u8], score: i64) {
        self.storage.set(group_id, member_id, score);
    }

    /// Returns member IDs whose score is at or below the removal threshold in a group.
    pub fn members_below_threshold(&self, group_id: &str) -> Vec<Vec<u8>> {
        self.storage
            .all_scores(group_id)
            .into_iter()
            .filter(|(_, score)| *score <= self.config.removal_threshold)
            .map(|(id, _)| id)
            .collect()
    }

    /// Check whether a specific member is at or below the removal threshold in a group.
    pub fn is_below_threshold(&self, group_id: &str, member_id: &[u8]) -> bool {
        self.storage
            .get(group_id, member_id)
            .is_some_and(|s| s <= self.config.removal_threshold)
    }

    /// Returns all members and their scores for a group.
    pub fn all_members_with_scores(&self, group_id: &str) -> Vec<(Vec<u8>, i64)> {
        self.storage.all_scores(group_id)
    }

    /// Returns a reference to the scoring config.
    pub fn config(&self) -> &ScoringConfig {
        &self.config
    }
}
