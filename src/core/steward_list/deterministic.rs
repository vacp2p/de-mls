//! Deterministic SHA256-sort steward list — reference
//! [`super::StewardListPlugin`] implementation.

use crate::core::{
    DEFAULT_MAX_RETRIES, ElectionDecision, StewardList, StewardListConfig, StewardListEvent,
    StewardListPlugin, error::CoreError,
};

#[derive(Debug)]
pub struct DeterministicStewardList {
    list: Option<StewardList>,
    config: StewardListConfig,
    conversation_id: Vec<u8>,
    retry_round: u32,
    max_retries: u32,
}

impl DeterministicStewardList {
    /// Joiner-side: empty list. The coordinator fills it from the
    /// `ConversationSync` it receives after the welcome.
    pub fn empty(conversation_id: impl Into<Vec<u8>>, config: StewardListConfig) -> Self {
        Self {
            list: None,
            config,
            conversation_id: conversation_id.into(),
            retry_round: 0,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// Creator-side: bootstrap with the creator as the sole steward at
    /// epoch 0. No election, no retries.
    pub fn with_creator(
        conversation_id: impl Into<Vec<u8>>,
        creator_member_id: Vec<u8>,
        config: StewardListConfig,
    ) -> Result<Self, CoreError> {
        let conversation_id = conversation_id.into();
        let list = StewardList::generate(
            0,
            &conversation_id,
            &[creator_member_id],
            1,
            config.clone(),
            0,
        )?;
        Ok(Self {
            list: Some(list),
            config,
            conversation_id,
            retry_round: 0,
            max_retries: DEFAULT_MAX_RETRIES,
        })
    }
}

impl StewardListPlugin for DeterministicStewardList {
    fn config(&self) -> &StewardListConfig {
        &self.config
    }

    fn set_config(&mut self, config: StewardListConfig) {
        self.config = config;
    }

    fn current_list(&self) -> Option<&StewardList> {
        self.list.as_ref()
    }

    fn election_epoch(&self) -> Option<u64> {
        self.list.as_ref().map(|l| l.election_epoch())
    }

    fn retry_round(&self) -> u32 {
        self.retry_round
    }

    fn max_retries(&self) -> u32 {
        self.max_retries
    }

    fn set_max_retries(&mut self, max: u32) {
        self.max_retries = max;
    }

    fn is_steward(&self, member_id: &[u8]) -> bool {
        self.list.as_ref().is_some_and(|l| l.contains(member_id))
    }

    fn is_exhausted(&self, epoch: u64) -> bool {
        self.list.as_ref().is_some_and(|l| l.is_exhausted(epoch))
    }

    fn epoch_steward<F: Fn(&[u8]) -> bool>(&self, epoch: u64, eligible: F) -> Option<&[u8]> {
        self.list
            .as_ref()
            .and_then(|l| l.epoch_steward(epoch, eligible))
    }

    fn epoch_and_backup<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        eligible: F,
    ) -> (Option<&[u8]>, Option<&[u8]>) {
        match self.list.as_ref() {
            Some(l) => l.epoch_and_backup(epoch, eligible),
            None => (None, None),
        }
    }

    fn steward_members<F: Fn(&[u8]) -> bool>(&self, eligible: F) -> Vec<Vec<u8>> {
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

    fn election_proposer<F: Fn(&[u8]) -> bool>(&self, eligible: F) -> Option<&[u8]> {
        // Election proposer = nominal index 0 = the steward whose
        // rotation slot covers `election_epoch` itself.
        self.list
            .as_ref()
            .and_then(|l| l.epoch_steward(l.election_epoch(), eligible))
    }

    fn install_list(
        &mut self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        sn: usize,
        retry_round: u32,
    ) -> Result<Vec<StewardListEvent>, CoreError> {
        let list = StewardList::generate(
            epoch,
            &self.conversation_id,
            candidate_pool,
            sn,
            self.config.clone(),
            retry_round,
        )?;
        let len = list.len();
        self.list = Some(list);
        Ok(vec![StewardListEvent::ListInstalled {
            epoch,
            retry_round,
            len,
        }])
    }

    fn validate_proposed(
        &self,
        proposed: &[Vec<u8>],
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        retry_round: u32,
    ) -> Result<bool, CoreError> {
        StewardList::validate(
            proposed,
            epoch,
            &self.conversation_id,
            candidate_pool,
            &self.config,
            retry_round,
        )
    }

    fn propose_election<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        self_member_id: &[u8],
        eligible: F,
        recovery: bool,
    ) -> Result<ElectionDecision, CoreError> {
        if !recovery && !self.is_exhausted(epoch) {
            return Ok(ElectionDecision::Skip("steward list not exhausted"));
        }
        let is_authorized = self
            .election_proposer(&eligible)
            .is_some_and(|proposer| proposer == self_member_id);
        if !is_authorized {
            return Ok(ElectionDecision::Skip("not the responsible proposer"));
        }
        if candidate_pool.is_empty() {
            return Ok(ElectionDecision::Skip(
                "no eligible candidates after filter",
            ));
        }

        let retry_round = self.retry_round();
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

    fn maybe_auto_fill(
        &mut self,
        epoch: u64,
        members: &[Vec<u8>],
    ) -> Result<Vec<StewardListEvent>, CoreError> {
        // RFC §Steward list creation: when total membership drops below
        // `sn_min`, every member must be a steward. Re-derive over the
        // full member set with `retry_round = 0` (auto-fill is not an
        // election outcome).
        if members.len() >= self.config.sn_min {
            return Ok(Vec::new());
        }
        let sn = self.config.compute_list_size(members.len());
        self.install_list(epoch, members, sn, 0)
    }

    fn bump_retry(&mut self) -> Vec<StewardListEvent> {
        self.retry_round = self.retry_round.saturating_add(1);
        if self.retry_round > self.max_retries {
            vec![StewardListEvent::RetryExhausted {
                round: self.retry_round,
                max: self.max_retries,
            }]
        } else {
            Vec::new()
        }
    }

    fn reset_retry(&mut self) {
        self.retry_round = 0;
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
    fn empty_plugin_has_no_list() {
        let p = DeterministicStewardList::empty(b"g".to_vec(), config());
        assert!(p.current_list().is_none());
        assert!(!p.is_steward(&member(1)));
        assert!(!p.is_exhausted(0));
        assert_eq!(p.epoch_steward(0, |_: &[u8]| true), None);
        assert_eq!(p.election_epoch(), None);
    }

    #[test]
    fn creator_bootstrap_makes_creator_a_steward() {
        let creator = member(1);
        let p = DeterministicStewardList::with_creator(b"g".to_vec(), creator.clone(), config())
            .unwrap();
        assert!(p.is_steward(&creator));
        assert_eq!(p.election_epoch(), Some(0));
        assert_eq!(p.current_list().unwrap().len(), 1);
    }

    #[test]
    fn install_emits_list_installed_event() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let events = p.install_list(0, &mems, 3, 0).unwrap();
        assert_eq!(
            events,
            vec![StewardListEvent::ListInstalled {
                epoch: 0,
                retry_round: 0,
                len: 3,
            }]
        );
        assert_eq!(p.current_list().unwrap().len(), 3);
        assert_eq!(p.election_epoch(), Some(0));
    }

    /// Filtering the nominal epoch steward out forces rotation to walk to
    /// the next eligible candidate.
    #[test]
    fn epoch_steward_filters_by_eligibility() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let nominal = p.epoch_steward(0, |_: &[u8]| true).unwrap().to_vec();
        let next = p
            .epoch_steward(0, |c: &[u8]| c != nominal.as_slice())
            .unwrap();
        assert_ne!(next, nominal.as_slice());
        assert!(mems.iter().any(|m| m == next));
    }

    #[test]
    fn epoch_and_backup_distinct_when_two_eligible() {
        let mut p =
            DeterministicStewardList::empty(b"g".to_vec(), StewardListConfig::new(3, 3).unwrap());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let (e, b) = p.epoch_and_backup(0, |_: &[u8]| true);
        assert!(e.is_some() && b.is_some());
        assert_ne!(e.unwrap(), b.unwrap());
    }

    /// Single eligible survivor → epoch resolves, backup stays None
    /// (nothing to be distinct from).
    #[test]
    fn backup_is_none_when_only_one_eligible() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let survivor = mems[0].clone();
        let (e, b) = p.epoch_and_backup(0, |c: &[u8]| c == survivor.as_slice());
        assert_eq!(e.unwrap(), survivor.as_slice());
        assert!(b.is_none());
    }

    /// `bump_retry` past `max_retries` emits `RetryExhausted` exactly
    /// once; default max is 1, so the second bump triggers it.
    #[test]
    fn bump_retry_emits_exhausted_after_max() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        assert!(p.bump_retry().is_empty());
        assert_eq!(p.retry_round(), 1);

        let events = p.bump_retry();
        assert_eq!(
            events,
            vec![StewardListEvent::RetryExhausted { round: 2, max: 1 }]
        );
        assert_eq!(p.retry_round(), 2);
    }

    #[test]
    fn reset_retry_clears_round() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let _ = p.bump_retry();
        let _ = p.bump_retry();
        assert_eq!(p.retry_round(), 2);
        p.reset_retry();
        assert_eq!(p.retry_round(), 0);
    }

    #[test]
    fn validate_proposed_against_self_derived_list() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();
        let proposed = p.current_list().unwrap().members().to_vec();
        assert!(p.validate_proposed(&proposed, 0, &mems, 0).unwrap());
    }

    #[test]
    fn validate_proposed_rejects_tampered_order() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();
        let mut tampered = p.current_list().unwrap().members().to_vec();
        tampered.swap(0, 1);
        assert!(!p.validate_proposed(&tampered, 0, &mems, 0).unwrap());
    }

    #[test]
    fn steward_members_returns_filtered_roster() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();
        let dropped = mems[0].clone();
        let filtered = p.steward_members(|c: &[u8]| c != dropped.as_slice());
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|m| m != &dropped));
    }

    #[test]
    fn set_max_retries_updates_threshold() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        p.set_max_retries(3);
        assert_eq!(p.max_retries(), 3);

        for _ in 0..3 {
            assert!(p.bump_retry().is_empty());
        }
        let events = p.bump_retry();
        assert_eq!(
            events,
            vec![StewardListEvent::RetryExhausted { round: 4, max: 3 }]
        );
    }

    #[test]
    fn election_proposer_walks_eligibility() {
        let mut p =
            DeterministicStewardList::empty(b"g".to_vec(), StewardListConfig::new(3, 3).unwrap());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let proposer = p.election_proposer(|_: &[u8]| true).unwrap().to_vec();
        let next = p
            .election_proposer(|c: &[u8]| c != proposer.as_slice())
            .unwrap();
        assert_ne!(next, proposer.as_slice());
    }

    /// When membership drops below `sn_min`, `maybe_auto_fill` regenerates
    /// the steward list over the full member set with `retry_round = 0`.
    /// RFC §"Steward list creation": every member must be a steward once
    /// the conversation is below `sn_min`.
    #[test]
    fn maybe_auto_fill_installs_full_member_set_when_below_sn_min() {
        let cfg = StewardListConfig::new(3, 5).unwrap();
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), cfg);
        let mems = members(&[1, 2]); // below sn_min = 3

        let events = p.maybe_auto_fill(5, &mems).unwrap();
        assert_eq!(
            events,
            vec![StewardListEvent::ListInstalled {
                epoch: 5,
                retry_round: 0,
                len: 2,
            }]
        );

        let list = p.current_list().expect("auto-fill installed a list");
        assert_eq!(list.len(), 2);
        assert_eq!(list.retry_round(), 0);
        for m in &mems {
            assert!(list.contains(m), "auto-filled list must cover every member");
        }
    }

    /// When membership is at or above `sn_min`, `maybe_auto_fill` is a
    /// no-op: no events emitted and any existing list is left untouched.
    #[test]
    fn maybe_auto_fill_no_op_when_at_or_above_sn_min() {
        let cfg = StewardListConfig::new(2, 5).unwrap();
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), cfg);
        let mems = members(&[1, 2, 3]); // ≥ sn_min = 2

        let events = p.maybe_auto_fill(0, &mems).unwrap();
        assert!(events.is_empty(), "no-op above sn_min");
        assert!(p.current_list().is_none(), "no list installed");
    }
}
