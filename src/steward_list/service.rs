//! [`StewardListService`] — the library-owned, SHA256-deterministic steward list
//! for one conversation.
//!
//! It owns the steward-list protocol: generating and validating the
//! deterministic list, rotating the epoch/backup steward, and proposing
//! elections. The integrator supplies only a [`StewardListConfig`].

use std::fmt::{Display, Formatter};

use crate::error::ConversationError;
use crate::steward_list::list::{StewardList, StewardListConfig};

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
    /// This node isn't the deterministically-responsible proposer.
    NotResponsibleProposer,
    /// No eligible candidates remain after the eligibility filter.
    NoEligibleCandidates,
}

impl Display for ElectionSkip {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotResponsibleProposer => "not the responsible proposer",
            Self::NoEligibleCandidates => "no eligible candidates after filter",
        })
    }
}

/// Per-conversation steward list. Eligibility flows in via `Fn(&[u8]) -> bool`
/// on position queries.
#[derive(Debug)]
pub struct StewardListService {
    list: Option<StewardList>,
    config: StewardListConfig,
    conversation_id: Vec<u8>,
}

impl StewardListService {
    /// Empty — no list until the library installs one (the creator's
    /// bootstrap list or a joiner's `ConversationSync`). The deterministic-sort
    /// salt is seeded by the library via [`Self::set_conversation_id`] once the
    /// conversation id is known, so the integrator builds this without it.
    pub fn empty(config: StewardListConfig) -> Self {
        Self {
            list: None,
            config,
            conversation_id: Vec::new(),
        }
    }

    /// The size bounds this conversation runs under (`sn_min`/`sn_max`).
    pub fn config(&self) -> &StewardListConfig {
        &self.config
    }

    /// Adopt conversation-wide bounds; keeps the installed list.
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

    /// The member authorized to propose the election: entry `round % len` of
    /// the authority sequence — the installed list's eligible members in
    /// rotation order, followed by the remaining eligible candidates in
    /// ascending id order. Round 0 is the eligible steward at the list's
    /// election-epoch slot. `None` when nobody is eligible.
    pub fn responsible_proposer<'a, F: Fn(&[u8]) -> bool>(
        &'a self,
        round: u32,
        candidate_pool: &'a [Vec<u8>],
        eligible: F,
    ) -> Option<&'a [u8]> {
        let list_members = self.list.as_ref().map(|l| l.members()).unwrap_or(&[]);
        let mut sequence: Vec<&[u8]> = list_members
            .iter()
            .map(Vec::as_slice)
            .filter(|c| eligible(c))
            .collect();
        let mut pool_rest: Vec<&[u8]> = candidate_pool
            .iter()
            .map(Vec::as_slice)
            .filter(|c| eligible(c) && !sequence.contains(c))
            .collect();
        pool_rest.sort_unstable();
        sequence.append(&mut pool_rest);
        if sequence.is_empty() {
            return None;
        }
        Some(sequence[round as usize % sequence.len()])
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
    /// a freshly generated list when this node is the responsible proposer,
    /// otherwise `Skip` with the reason. The caller decides whether an
    /// election is due (natural exhaustion, or an application request); this
    /// always proposes when called.
    pub fn propose_election<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        self_member_id: &[u8],
        eligible: F,
    ) -> Result<ElectionDecision, ConversationError> {
        if candidate_pool.is_empty() {
            return Ok(ElectionDecision::Skip(ElectionSkip::NoEligibleCandidates));
        }
        let is_authorized = self
            .responsible_proposer(0, candidate_pool, &eligible)
            .is_some_and(|proposer| proposer == self_member_id);
        if !is_authorized {
            return Ok(ElectionDecision::Skip(ElectionSkip::NotResponsibleProposer));
        }

        let sn = self.config.compute_list_size(candidate_pool.len());
        let list = StewardList::generate(
            epoch,
            &self.conversation_id,
            candidate_pool,
            sn,
            self.config.clone(),
            0,
        )?;
        Ok(ElectionDecision::Proposed {
            proposed_stewards: list.members().to_vec(),
            election_epoch: epoch,
            retry_round: 0,
        })
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

    /// Round 0 hands authority to the eligible steward at the list's
    /// election-epoch slot.
    #[test]
    fn responsible_proposer_round_zero_is_the_election_epoch_steward() {
        let mut p = StewardListService::empty(StewardListConfig::new(3, 3).unwrap());
        let mems = members(&[1, 2, 3]);
        p.install_list(0, &mems, 3, 0).unwrap();

        let list = p.current_list().unwrap();
        assert_eq!(
            p.responsible_proposer(0, &mems, |_: &[u8]| true),
            list.epoch_steward(list.election_epoch(), |_: &[u8]| true),
        );
    }

    #[test]
    fn responsible_proposer_falls_back_past_ineligible_list() {
        // The whole installed list is ineligible (e.g. its sole steward is
        // unresponsive): authority falls back to the lowest eligible pool id.
        let mut p = StewardListService::empty(StewardListConfig::new(1, 1).unwrap());
        let listed = members(&[9]);
        p.install_list(0, &listed, 1, 0).unwrap();

        let pool = members(&[3, 2, 9]);
        let proposer = p
            .responsible_proposer(0, &pool, |c: &[u8]| c != member(9).as_slice())
            .unwrap();
        assert_eq!(proposer, member(2).as_slice());
    }

    #[test]
    fn responsible_proposer_rotates_by_round() {
        // Authority sequence: eligible list members in list order, then the
        // remaining eligible pool ids ascending; the round indexes it (mod
        // len).
        let mut p = StewardListService::empty(StewardListConfig::new(1, 1).unwrap());
        let listed = members(&[9]);
        p.install_list(0, &listed, 1, 0).unwrap();

        let pool = members(&[3, 2, 9]);
        let all = |_: &[u8]| true;
        assert_eq!(p.responsible_proposer(0, &pool, all).unwrap(), member(9));
        assert_eq!(p.responsible_proposer(1, &pool, all).unwrap(), member(2));
        assert_eq!(p.responsible_proposer(2, &pool, all).unwrap(), member(3));
        assert_eq!(p.responsible_proposer(3, &pool, all).unwrap(), member(9));
    }

    #[test]
    fn responsible_proposer_none_when_no_one_eligible() {
        let mut p = StewardListService::empty(StewardListConfig::new(1, 1).unwrap());
        let listed = members(&[9]);
        p.install_list(0, &listed, 1, 0).unwrap();

        let pool = members(&[2]);
        assert_eq!(p.responsible_proposer(0, &pool, |_: &[u8]| false), None);
    }

    #[test]
    fn election_required_only_above_sn_max() {
        let cfg = StewardListConfig::new(2, 3).unwrap();
        let p = StewardListService::empty(cfg);
        assert!(!p.election_required(1));
        assert!(!p.election_required(3));
        assert!(p.election_required(4));
    }
}
