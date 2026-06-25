//! [`StewardListService`] — the library-owned, SHA256-deterministic steward
//! roster for one conversation.
//!
//! It owns the steward-list protocol: generating and validating the
//! deterministic list, rotating the epoch/backup steward, proposing elections,
//! and tracking retry rounds. The integrator supplies only a
//! [`StewardListConfig`].

use std::fmt::{Display, Formatter};

use crate::error::ConversationError;
use crate::steward_list::list::{StewardList, StewardListConfig};

/// Steward-election retries *after* the initial attempt, before `Deadlock`
/// escalation. So `1` = two attempts (rounds 0 and 1) total.
pub const DEFAULT_MAX_RETRIES: u32 = 1;

/// Result of [`StewardListService::propose_election`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElectionDecision {
    Skip(ElectionSkip),
    Proposed {
        proposed_stewards: Vec<Vec<u8>>,
        election_epoch: u64,
        retry_round: u32,
    },
}

/// Why [`StewardListService::propose_election`] declined to propose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElectionSkip {
    /// The current list still covers this epoch — no election due yet.
    NotExhausted,
    /// This node isn't the deterministically-responsible proposer.
    NotResponsibleProposer,
    /// No eligible candidates remain after the eligibility filter.
    NoEligibleCandidates,
}

impl Display for ElectionSkip {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotExhausted => "steward list not exhausted",
            Self::NotResponsibleProposer => "not the responsible proposer",
            Self::NoEligibleCandidates => "no eligible candidates after filter",
        })
    }
}

/// Restorable state of a [`StewardListService`]: the installed list and the
/// retry counters. The conversation-id salt isn't stored — the library re-seeds
/// it at restore. Pulled by-request for the integrator to fold into
/// a conversation snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StewardListSnapshot {
    list: StewardList,
    next_election_round: u32,
    max_retries: u32,
}

/// Per-conversation steward roster. Eligibility flows in via `Fn(&[u8]) -> bool`
/// on position queries.
#[derive(Debug)]
pub struct StewardListService {
    list: Option<StewardList>,
    config: StewardListConfig,
    conversation_id: Vec<u8>,
    /// The live retry counter: the seed the *next* election attempt will feed
    /// into generation. Bumped on a rejected election, reset to 0 on success.
    /// Distinct from a [`StewardList`]'s own `retry_round`, which records the
    /// seed an already-elected list was built with.
    next_election_round: u32,
    max_retries: u32,
}

impl StewardListService {
    /// Empty roster — no list until the library installs one (the creator's
    /// bootstrap list or a joiner's `ConversationSync`). The deterministic-sort
    /// salt is seeded by the library via [`Self::set_conversation_id`] once the
    /// conversation id is known, so the integrator builds this without it.
    pub fn empty(config: StewardListConfig) -> Self {
        Self {
            list: None,
            config,
            conversation_id: Vec::new(),
            next_election_round: 0,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// The size bounds this conversation runs under (`sn_min`/`sn_max` and
    /// `allow_subset_candidates`).
    pub fn config(&self) -> &StewardListConfig {
        &self.config
    }

    /// Adopt conversation-wide bounds; keeps list and retry state.
    pub fn set_config(&mut self, config: StewardListConfig) {
        self.config = config;
    }

    /// Seed the deterministic-sort salt with the conversation id. Every member
    /// of a conversation must use the same salt or their generated/validated
    /// lists diverge on a subset election. The library sets this when it builds
    /// the conversation.
    pub fn set_conversation_id(&mut self, conversation_id: &[u8]) {
        self.conversation_id = conversation_id.to_vec();
    }

    /// The active list, or `None` before one is installed (a creator installs at
    /// bootstrap; a joiner on `ConversationSync`).
    pub fn current_list(&self) -> Option<&StewardList> {
        self.list.as_ref()
    }

    /// Epoch the active list was elected for, or `None` if no list is installed.
    pub fn election_epoch(&self) -> Option<u64> {
        self.list.as_ref().map(|l| l.election_epoch())
    }

    /// Round the *next* election attempt will use as its generation seed.
    /// Distinct from [`StewardList::retry_round`] — the seed frozen into the
    /// current list.
    pub fn next_election_round(&self) -> u32 {
        self.next_election_round
    }

    /// Retry cap before an unanswered election escalates to `Deadlock`.
    pub fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// Set the retry cap. The library applies `ConversationConfig.max_reelection_attempts`.
    pub fn set_max_retries(&mut self, max: u32) {
        self.max_retries = max;
    }

    /// Whether `member_id` is on the active list.
    pub fn is_steward(&self, member_id: &[u8]) -> bool {
        self.list.as_ref().is_some_and(|l| l.contains(member_id))
    }

    /// Whether `epoch` is outside the active list's `[election_epoch, +len)`
    /// span. `false` when no list is installed.
    pub fn is_exhausted(&self, epoch: u64) -> bool {
        self.list.as_ref().is_some_and(|l| l.is_exhausted(epoch))
    }

    /// Whether a steward list over `member_count` members needs a *voted*
    /// election. `false` while every member is a steward (`member_count <=
    /// sn_max`): the list is then the full membership, fully determined and
    /// regenerated deterministically with no consensus. `true` once the list
    /// is a genuine subset, where peers must agree which members serve.
    pub fn election_required(&self, member_count: usize) -> bool {
        member_count > self.config.sn_max
    }

    /// The steward serving `epoch`, skipping slots the `eligible` predicate
    /// rejects. `None` with no list or an exhausted one. See
    /// [`StewardList::epoch_steward`].
    pub fn epoch_steward<F: Fn(&[u8]) -> bool>(&self, epoch: u64, eligible: F) -> Option<&[u8]> {
        self.list
            .as_ref()
            .and_then(|l| l.epoch_steward(epoch, eligible))
    }

    /// The epoch steward and its backup for `epoch`, or `(None, None)` with no
    /// list. See [`StewardList::epoch_and_backup`].
    pub fn epoch_and_backup<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        eligible: F,
    ) -> (Option<&[u8]>, Option<&[u8]>) {
        match self.list.as_ref() {
            Some(l) => l.epoch_and_backup(epoch, eligible),
            None => (None, None),
        }
    }

    /// Eligible stewards on the active list (for `ConversationSync.steward_members`).
    pub fn steward_members<F: Fn(&[u8]) -> bool>(&self, eligible: F) -> Vec<Vec<u8>> {
        self.list
            .as_ref()
            .map(|l| {
                l.members()
                    .iter()
                    .filter(|m| eligible(m))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The member responsible for proposing an election: the first eligible
    /// steward at the list's election-epoch slot. `None` with no list. Used to
    /// authorize a single proposer and recorded in `ConversationSync`.
    pub fn election_proposer<F: Fn(&[u8]) -> bool>(&self, eligible: F) -> Option<&[u8]> {
        self.list
            .as_ref()
            .and_then(|l| l.epoch_steward(l.election_epoch(), eligible))
    }

    /// Build and install a list of size `sn`. `retry_round` seeds the SHA256 sort
    /// and is stored on the list; use `0` for bootstrap and deterministic regen.
    pub fn install_list(
        &mut self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        sn: usize,
        retry_round: u32,
    ) -> Result<(), ConversationError> {
        let list = StewardList::generate(
            epoch,
            &self.conversation_id,
            candidate_pool,
            sn,
            self.config.clone(),
            retry_round,
        )?;
        self.list = Some(list);
        Ok(())
    }

    /// `true` if `proposed` matches [`StewardList::validate`] for these parameters.
    pub fn validate_proposed(
        &self,
        proposed: &[Vec<u8>],
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        retry_round: u32,
    ) -> Result<bool, ConversationError> {
        StewardList::validate(
            proposed,
            epoch,
            &self.conversation_id,
            candidate_pool,
            &self.config,
            retry_round,
        )
    }

    /// Decide whether to propose a steward election at `epoch`: `Proposed` with
    /// a freshly generated list when this node is the responsible proposer and
    /// an election is due, otherwise `Skip` with the reason. `recovery` forces a
    /// proposal even when the list isn't exhausted (deadlock recovery).
    pub fn propose_election<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        self_member_id: &[u8],
        eligible: F,
        recovery: bool,
    ) -> Result<ElectionDecision, ConversationError> {
        if !recovery && !self.is_exhausted(epoch) {
            return Ok(ElectionDecision::Skip(ElectionSkip::NotExhausted));
        }
        let is_authorized = self
            .election_proposer(&eligible)
            .is_some_and(|proposer| proposer == self_member_id);
        if !is_authorized {
            return Ok(ElectionDecision::Skip(ElectionSkip::NotResponsibleProposer));
        }
        if candidate_pool.is_empty() {
            return Ok(ElectionDecision::Skip(ElectionSkip::NoEligibleCandidates));
        }

        let retry_round = self.next_election_round();
        let sn = self.config.compute_list_size(candidate_pool.len());
        let list = StewardList::generate(
            epoch,
            &self.conversation_id,
            candidate_pool,
            sn,
            self.config.clone(),
            retry_round,
        )?;
        Ok(ElectionDecision::Proposed {
            proposed_stewards: list.members().to_vec(),
            election_epoch: epoch,
            retry_round,
        })
    }

    /// Increment the retry round. Exhaustion is detected by the caller via
    /// [`Self::next_election_round`] vs [`Self::max_retries`].
    pub fn bump_retry(&mut self) {
        self.next_election_round = self.next_election_round.saturating_add(1);
    }

    /// Reset the retry round to 0 — a fresh election landed.
    pub fn reset_retry(&mut self) {
        self.next_election_round = 0;
    }

    /// Snapshot the installed list and retry state for persistence. Pulled
    /// by-request; the conversation-id salt isn't captured (re-seeded at
    /// restore). Errors when no list is installed — there's nothing to capture.
    pub fn snapshot(&self) -> Result<StewardListSnapshot, ConversationError> {
        if self.list.is_none() {
            return Err(ConversationError::EmptyMembersList);
        }
        Ok(StewardListSnapshot {
            list: self.list.clone().unwrap(),
            next_election_round: self.next_election_round,
            max_retries: self.max_retries,
        })
    }

    /// Restore state captured by [`Self::snapshot`]. Call
    /// [`Self::set_conversation_id`] first if a later subset election must
    /// regenerate against the same salt.
    pub fn restore(&mut self, snapshot: StewardListSnapshot) {
        self.list = Some(snapshot.list);
        self.next_election_round = snapshot.next_election_round;
        self.max_retries = snapshot.max_retries;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    fn config() -> StewardListConfig {
        StewardListConfig::new(1, 5).unwrap()
    }

    #[test]
    fn empty_service_has_no_list() {
        let p = StewardListService::empty(config());
        assert!(p.current_list().is_none());
        assert!(!p.is_steward(&member(1)));
        assert!(!p.is_exhausted(0));
        assert_eq!(p.epoch_steward(0, |_: &[u8]| true), None);
        assert_eq!(p.election_epoch(), None);
    }

    #[test]
    fn install_sets_current_list() {
        let mut p = StewardListService::empty(config());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();
        assert_eq!(p.current_list().unwrap().len(), 3);
        assert_eq!(p.election_epoch(), Some(0));
    }

    #[test]
    fn epoch_steward_filters_by_eligibility() {
        let mut p = StewardListService::empty(config());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();

        let nominal = p.epoch_steward(0, |_: &[u8]| true).unwrap().to_vec();
        let next = p
            .epoch_steward(0, |c: &[u8]| c != nominal.as_slice())
            .unwrap();
        assert_ne!(next, nominal.as_slice());
        assert!(mems.iter().any(|m| m == next));
    }

    #[test]
    fn epoch_and_backup_distinct_when_two_eligible() {
        let mut p = StewardListService::empty(StewardListConfig::new(3, 3).unwrap());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();

        let (e, b) = p.epoch_and_backup(0, |_: &[u8]| true);
        assert!(e.is_some() && b.is_some());
        assert_ne!(e.unwrap(), b.unwrap());
    }

    #[test]
    fn backup_is_none_when_only_one_eligible() {
        let mut p = StewardListService::empty(config());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();

        let survivor = mems[0].clone();
        let (e, b) = p.epoch_and_backup(0, |c: &[u8]| c == survivor.as_slice());
        assert_eq!(e.unwrap(), survivor.as_slice());
        assert!(b.is_none());
    }

    #[test]
    fn bump_retry_increments_round_past_max() {
        let mut p = StewardListService::empty(config());
        p.bump_retry();
        assert_eq!(p.next_election_round(), 1);
        assert!(p.next_election_round() <= p.max_retries());

        p.bump_retry();
        assert_eq!(p.next_election_round(), 2);
        assert!(
            p.next_election_round() > p.max_retries(),
            "round 2 exceeds max 1"
        );
    }

    #[test]
    fn reset_retry_clears_round() {
        let mut p = StewardListService::empty(config());
        p.bump_retry();
        p.bump_retry();
        assert_eq!(p.next_election_round(), 2);
        p.reset_retry();
        assert_eq!(p.next_election_round(), 0);
    }

    #[test]
    fn validate_proposed_against_self_derived_list() {
        let mut p = StewardListService::empty(config());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();
        let proposed = p.current_list().unwrap().members().to_vec();
        assert!(p.validate_proposed(&proposed, 0, &mems, 0).unwrap());
    }

    #[test]
    fn validate_proposed_rejects_tampered_order() {
        let mut p = StewardListService::empty(config());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();
        let mut tampered = p.current_list().unwrap().members().to_vec();
        tampered.swap(0, 1);
        assert!(!p.validate_proposed(&tampered, 0, &mems, 0).unwrap());
    }

    #[test]
    fn steward_members_returns_filtered_roster() {
        let mut p = StewardListService::empty(config());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();
        let dropped = mems[0].clone();
        let filtered = p.steward_members(|c: &[u8]| c != dropped.as_slice());
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|m| m != &dropped));
    }

    #[test]
    fn set_max_retries_updates_threshold() {
        let mut p = StewardListService::empty(config());
        p.set_max_retries(3);
        assert_eq!(p.max_retries(), 3);

        for _ in 0..3 {
            p.bump_retry();
            assert!(p.next_election_round() <= p.max_retries());
        }
        p.bump_retry();
        assert!(
            p.next_election_round() > p.max_retries(),
            "round 4 exceeds max 3"
        );
    }

    #[test]
    fn election_proposer_walks_eligibility() {
        let mut p = StewardListService::empty(StewardListConfig::new(3, 3).unwrap());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();

        let proposer = p.election_proposer(|_: &[u8]| true).unwrap().to_vec();
        let next = p
            .election_proposer(|c: &[u8]| c != proposer.as_slice())
            .unwrap();
        assert_ne!(next, proposer.as_slice());
    }

    #[test]
    fn election_required_only_above_sn_max() {
        let cfg = StewardListConfig::new(2, 3).unwrap();
        let p = StewardListService::empty(cfg);
        assert!(!p.election_required(1));
        assert!(!p.election_required(3));
        assert!(p.election_required(4));
    }

    #[test]
    fn snapshot_restore_round_trips_list_and_retry() {
        let mut p = StewardListService::empty(config());
        p.set_conversation_id(b"conv");
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();
        p.bump_retry();

        let snap = p.snapshot().unwrap();
        let mut restored = StewardListService::empty(config());
        restored.set_conversation_id(b"conv");
        restored.restore(snap);

        assert_eq!(
            restored.current_list().unwrap().members(),
            p.current_list().unwrap().members()
        );
        assert_eq!(restored.election_epoch(), Some(0));
        assert_eq!(restored.next_election_round(), 1);
    }

    #[test]
    fn snapshot_of_empty_service_errors() {
        // A roster with no installed list has nothing to snapshot.
        let p = StewardListService::empty(config());
        assert!(p.snapshot().is_err());
    }
}
