//! The deterministic steward list — who serves as steward, and in what order.
//!
//! [`StewardListConfig`] fixes the size bounds (`sn_min`/`sn_max`);
//! [`StewardList`] is one elected, ordered roster. Stewards are chosen by
//! ascending `SHA256(election_epoch || retry_round || member_id ||
//! conversation_id)`, keeping the first `sn` — the same key every member
//! computes, so the list is reproducible and unbiased (RFC §"Steward list
//! creation"). Each epoch shifts the active slot by one, rotating the
//! epoch/backup steward.

use std::ops::RangeInclusive;

use sha2::{Digest, Sha256};

use crate::error::ConversationError;

/// Group-wide steward-list size bounds, fixed at conversation creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StewardListConfig {
    /// Smallest list size, applied once membership reaches it. Below `sn_min`
    /// members, the list is the whole group.
    pub sn_min: usize,
    /// Largest list size. Past this, the list is a genuine subset and members
    /// must vote on who serves.
    pub sn_max: usize,
    /// Whether an election may draw candidates from a subset of members rather
    /// than the full roster. Read by the coordinator; not used in generation.
    pub allow_subset_candidates: bool,
}

impl Default for StewardListConfig {
    fn default() -> Self {
        Self {
            sn_min: 1,
            sn_max: 2,
            allow_subset_candidates: false,
        }
    }
}

impl StewardListConfig {
    /// Validated constructor: requires `1 <= sn_min <= sn_max`.
    pub fn new(sn_min: usize, sn_max: usize) -> Result<Self, ConversationError> {
        if sn_min < 1 || sn_min > sn_max {
            return Err(ConversationError::InvalidConfigSize);
        }
        Ok(Self {
            sn_min,
            sn_max,
            allow_subset_candidates: false,
        })
    }

    /// List size to elect for `total_members` — the largest legal size.
    pub fn compute_list_size(&self, total_members: usize) -> usize {
        *self.size_bounds(total_members).end()
    }

    /// Whether `size` is a legal list size for a group of `total_members`.
    pub fn is_valid_size(&self, size: usize, total_members: usize) -> bool {
        self.size_bounds(total_members).contains(&size)
    }

    /// Valid size range for `total_members`: the exact count when the group is
    /// smaller than `sn_min`, otherwise `[sn_min, min(sn_max, total_members)]`.
    fn size_bounds(&self, total_members: usize) -> RangeInclusive<usize> {
        if total_members < self.sn_min {
            total_members..=total_members
        } else {
            self.sn_min..=self.sn_max.min(total_members)
        }
    }

    /// Reject inputs `generate`/`validate` can't honor: an empty roster, or an `sn`
    /// outside the config's valid range for this membership.
    pub fn check_generation_inputs(
        &self,
        member_ids: &[Vec<u8>],
        sn: usize,
    ) -> Result<(), ConversationError> {
        if member_ids.is_empty() {
            return Err(ConversationError::EmptyMembersList);
        }
        if !self.is_valid_size(sn, member_ids.len()) {
            return Err(ConversationError::InvalidConfigSize);
        }
        Ok(())
    }
}

/// One elected, ordered steward roster, serving epochs
/// `[election_epoch, election_epoch + len)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StewardList {
    members: Vec<Vec<u8>>,
    config: StewardListConfig,
    election_epoch: u64,
    /// The retry round this list was generated with — the seed baked into its
    /// SHA256 sort. Frozen provenance, so any peer can re-derive and validate
    /// the exact same list. Distinct from the service's live
    /// `next_election_round` counter.
    retry_round: u32,
}

impl StewardList {
    /// Elect `sn` stewards: sort `member_ids` by ascending steward hash and keep
    /// the first `sn`. Deterministic — every member derives the same list.
    /// Errors on an empty roster or an `sn` outside the config bounds.
    pub fn generate(
        election_epoch: u64,
        conversation_id: &[u8],
        member_ids: &[Vec<u8>],
        sn: usize,
        config: StewardListConfig,
        retry_round: u32,
    ) -> Result<Self, ConversationError> {
        config.check_generation_inputs(member_ids, sn)?;
        let ordered =
            sorted_steward_indices(election_epoch, retry_round, conversation_id, member_ids);
        let members = ordered
            .into_iter()
            .take(sn)
            .map(|i| member_ids[i].clone())
            .collect();
        Ok(Self {
            members,
            config,
            election_epoch,
            retry_round,
        })
    }

    /// Whether `proposed` is exactly what [`Self::generate`] would produce for
    /// these inputs — the check every member runs on an election proposal before
    /// accepting it. Compares against the freshly sorted order without cloning
    /// the member list.
    pub fn validate(
        proposed: &[Vec<u8>],
        election_epoch: u64,
        conversation_id: &[u8],
        member_ids: &[Vec<u8>],
        config: &StewardListConfig,
        retry_round: u32,
    ) -> Result<bool, ConversationError> {
        let sn = proposed.len();
        config.check_generation_inputs(member_ids, sn)?;
        let ordered =
            sorted_steward_indices(election_epoch, retry_round, conversation_id, member_ids);
        Ok(ordered
            .iter()
            .take(sn)
            .zip(proposed.iter())
            .all(|(&i, want)| &member_ids[i] == want))
    }

    /// The steward serving `epoch`, walking past any the `eligible` predicate
    /// rejects.
    /// `None` if the list is exhausted or no one is eligible.
    pub fn epoch_steward<F: Fn(&[u8]) -> bool>(&self, epoch: u64, eligible: F) -> Option<&[u8]> {
        self.steward_from(epoch, 0, eligible)
    }

    /// The epoch steward and its backup (the next eligible slot), distinct when
    /// two members are eligible. Either is `None` when unavailable.
    pub fn epoch_and_backup<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        eligible: F,
    ) -> (Option<&[u8]>, Option<&[u8]>) {
        let epoch_steward = self.steward_from(epoch, 0, &eligible);
        let backup =
            epoch_steward.and_then(|es| self.steward_from(epoch, 1, |c| c != es && eligible(c)));
        (epoch_steward, backup)
    }

    /// Walk the rotation from `epoch`'s slot plus `offset`, returning the first
    /// member `eligible` accepts. `None` if exhausted or none match.
    fn steward_from<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        offset: usize,
        eligible: F,
    ) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let len = self.members.len();
        let start = ((epoch - self.election_epoch) as usize + offset) % len;
        for step in 0..len {
            let idx = (start + step) % len;
            let candidate = &self.members[idx];
            if eligible(candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Whether `epoch` falls outside this list's `[election_epoch, +len)` span —
    /// before it starts or past its last assigned epoch.
    pub fn is_exhausted(&self, epoch: u64) -> bool {
        if epoch < self.election_epoch {
            return true;
        }
        (epoch - self.election_epoch) >= self.members.len() as u64
    }

    /// Whether `member_id` is on this list.
    pub fn contains(&self, member_id: &[u8]) -> bool {
        self.members.iter().any(|m| m.as_slice() == member_id)
    }

    /// The stewards in elected (rotation) order.
    pub fn members(&self) -> &[Vec<u8>] {
        &self.members
    }

    /// Number of stewards on the list.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the list has no stewards.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// The size bounds this list was elected under.
    pub fn config(&self) -> &StewardListConfig {
        &self.config
    }

    /// First epoch this list serves.
    pub fn election_epoch(&self) -> u64 {
        self.election_epoch
    }

    /// Retry round frozen into this list as its SHA256-sort seed (carried in
    /// `ConversationSync`). Distinct from the service's live `next_election_round`.
    pub fn retry_round(&self) -> u32 {
        self.retry_round
    }
}

/// Member indices ordered by ascending steward hash, tie-broken on `member_id`
/// so the order is total and independent of the input slice's ordering.
fn sorted_steward_indices(
    election_epoch: u64,
    retry_round: u32,
    conversation_id: &[u8],
    member_ids: &[Vec<u8>],
) -> Vec<usize> {
    let mut scored: Vec<(Vec<u8>, usize)> = member_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                compute_steward_hash(election_epoch, retry_round, id, conversation_id),
                i,
            )
        })
        .collect();
    // Tie-break on member_id so a hash collision is still cross-peer total,
    // not dependent on the caller's slice ordering.
    scored.sort_by(|(a, ai), (b, bi)| a.cmp(b).then_with(|| member_ids[*ai].cmp(&member_ids[*bi])));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// `SHA256(epoch || retry_round || member_id || conversation_id)` — the
/// per-member sort key.
fn compute_steward_hash(
    epoch: u64,
    retry_round: u32,
    member_id: &[u8],
    conversation_id: &[u8],
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(epoch.to_be_bytes());
    hasher.update(retry_round.to_be_bytes());
    hasher.update(member_id);
    hasher.update(conversation_id);
    hasher.finalize().to_vec()
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

    #[test]
    fn test_config_validation() {
        let config = StewardListConfig::new(2, 5).unwrap();

        // Within [sn_min, sn_max]
        assert!(config.is_valid_size(3, 10));
        assert!(config.is_valid_size(2, 10));
        assert!(config.is_valid_size(5, 10));
        assert!(!config.is_valid_size(1, 10));
        assert!(!config.is_valid_size(6, 10));

        // Fewer members than sn_min → only `size == total` is valid.
        assert!(config.is_valid_size(1, 1));
        assert!(!config.is_valid_size(2, 1));
    }

    #[test]
    fn test_new_rejects_bad_bounds() {
        assert!(StewardListConfig::new(0, 5).is_err(), "sn_min == 0");
        assert!(StewardListConfig::new(5, 3).is_err(), "sn_min > sn_max");
    }

    #[test]
    fn test_generate_empty_members() {
        let config = StewardListConfig::new(1, 3).unwrap();
        assert!(StewardList::generate(0, b"conversation1", &[], 1, config, 0).is_err());
    }

    #[test]
    fn test_generate_invalid_sn() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        assert!(
            StewardList::generate(0, b"conversation1", &mems, 1, config.clone(), 0).is_err(),
            "below sn_min"
        );
        assert!(
            StewardList::generate(0, b"conversation1", &mems, 6, config, 0).is_err(),
            "above sn_max"
        );
    }

    #[test]
    fn test_deterministic_generation() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let conversation_id = b"test-conversation";

        let list1 = StewardList::generate(0, conversation_id, &mems, 3, config.clone(), 0).unwrap();
        let list2 = StewardList::generate(0, conversation_id, &mems, 3, config, 0).unwrap();

        assert_eq!(list1.members(), list2.members());
        assert_eq!(list1.len(), 3);
    }

    #[test]
    fn test_different_epoch_shuffles() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let base = StewardList::generate(0, b"conversation", &mems, 5, config.clone(), 0).unwrap();
        let any_diff = (1..10).any(|e| {
            let other =
                StewardList::generate(e, b"conversation", &mems, 5, config.clone(), 0).unwrap();
            other.members() != base.members()
        });
        assert!(any_diff);
    }

    #[test]
    fn test_different_conversation_shuffles() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let base = StewardList::generate(0, b"conversation1", &mems, 5, config.clone(), 0).unwrap();
        let other = StewardList::generate(0, b"conversation2", &mems, 5, config, 0).unwrap();
        assert_ne!(base.members(), other.members());
    }

    #[test]
    fn test_member_order_does_not_affect_result() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems_a = members(&[1, 2, 3, 4, 5]);
        let mems_b = members(&[5, 3, 1, 4, 2]);

        let list_a =
            StewardList::generate(0, b"conversation", &mems_a, 3, config.clone(), 0).unwrap();
        let list_b = StewardList::generate(0, b"conversation", &mems_b, 3, config, 0).unwrap();

        assert_eq!(list_a.members(), list_b.members());
    }

    /// At the small-group boundary (`members == sn_max`) the regenerated
    /// list is the full membership: `compute_list_size` returns `sn_max` and
    /// `generate` keeps every member (only reordered). This is the invariant
    /// `reconcile_steward_list` relies on — the no-vote local regen yields
    /// the same set a successful election would.
    #[test]
    fn test_regen_at_sn_max_is_full_membership() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]); // len == sn_max
        let sn = config.compute_list_size(mems.len());
        assert_eq!(sn, 5, "size at the sn_max boundary is sn_max");

        let list = StewardList::generate(7, b"conv", &mems, sn, config, 0).unwrap();
        let got: std::collections::HashSet<_> = list.members().iter().cloned().collect();
        let want: std::collections::HashSet<_> = mems.iter().cloned().collect();
        assert_eq!(
            got, want,
            "every member is a steward at the sn_max boundary"
        );
    }

    #[test]
    fn test_epoch_steward_rotation() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"conversation", &mems, 3, config, 0).unwrap();
        let s0 = list.epoch_steward(0, |_| true).unwrap().to_vec();
        let s1 = list.epoch_steward(1, |_| true).unwrap().to_vec();
        let s2 = list.epoch_steward(2, |_| true).unwrap().to_vec();

        assert_ne!(s0, s1);
        assert_ne!(s1, s2);
        assert_ne!(s0, s2);
    }

    #[test]
    fn test_backup_is_next_rotation_slot() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"conversation", &mems, 3, config, 0).unwrap();
        let order: Vec<&[u8]> = list.members().iter().map(|m| m.as_slice()).collect();

        for epoch in 0..3u64 {
            let (epoch_steward, backup) = list.epoch_and_backup(epoch, |_| true);
            assert_eq!(epoch_steward, Some(order[epoch as usize % 3]));
            assert_eq!(backup, Some(order[(epoch as usize + 1) % 3]));
        }
    }

    #[test]
    fn test_list_exhaustion() {
        let config = StewardListConfig::new(2, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(5, b"conversation", &mems, 3, config, 0).unwrap();
        assert_eq!(list.election_epoch(), 5);

        assert!(!list.is_exhausted(5));
        assert!(!list.is_exhausted(7));
        assert!(list.is_exhausted(8));
        assert!(
            list.is_exhausted(4),
            "epochs before election_epoch are exhausted"
        );

        assert!(list.epoch_steward(8, |_| true).is_none());
        assert!(list.epoch_and_backup(8, |_| true).1.is_none());
    }

    #[test]
    fn test_validate_correct_list() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"conversation", &mems, 3, config.clone(), 0).unwrap();
        let valid = StewardList::validate(list.members(), 0, b"conversation", &mems, &config, 0);
        assert!(valid.is_ok());
        assert!(valid.unwrap())
    }

    #[test]
    fn test_validate_tampered_list() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let mut list =
            StewardList::generate(0, b"conversation", &mems, 3, config.clone(), 0).unwrap();
        list.members.swap(0, 1);

        let valid = StewardList::validate(list.members(), 0, b"conversation", &mems, &config, 0);
        assert!(valid.is_ok());
        assert!(!valid.unwrap())
    }

    #[test]
    fn test_validate_wrong_epoch() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"conversation", &mems, 5, config.clone(), 0).unwrap();
        let diff_epoch = (1..100u64)
            .find(|&e| {
                let o =
                    StewardList::generate(e, b"conversation", &mems, 5, config.clone(), 0).unwrap();
                o.members() != list.members()
            })
            .expect("should differ within 100 epochs");

        let valid = StewardList::validate(
            list.members(),
            diff_epoch,
            b"conversation",
            &mems,
            &config,
            0,
        );
        assert!(valid.is_ok());
        assert!(!valid.unwrap());
    }

    #[test]
    fn test_validate_wrong_members() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let other_mems = members(&[1, 2, 3, 4, 6]);

        let list = StewardList::generate(0, b"conversation", &mems, 5, config.clone(), 0).unwrap();
        let valid =
            StewardList::validate(list.members(), 0, b"conversation", &other_mems, &config, 0);
        assert!(valid.is_ok());
        assert!(!valid.unwrap())
    }

    #[test]
    fn test_single_member() {
        let config = StewardListConfig::new(1, 3).unwrap();
        let mems = members(&[1]);

        let list = StewardList::generate(0, b"conversation", &mems, 1, config, 0).unwrap();
        assert_eq!(list.len(), 1);
        let (e, b) = list.epoch_and_backup(0, |_| true);
        assert!(e.is_some());
        assert!(b.is_none());
        assert!(list.is_exhausted(1));
    }

    #[test]
    fn test_epoch_steward_walks_past_ineligible() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"conversation", &mems, 3, config, 0).unwrap();
        let nominal = list.epoch_steward(0, |_| true).unwrap().to_vec();

        let all_eligible = |c: &[u8]| mems.iter().any(|m| m == c);
        assert_eq!(
            list.epoch_steward(0, all_eligible),
            Some(nominal.as_slice())
        );

        let after: Vec<Vec<u8>> = mems.iter().filter(|m| **m != nominal).cloned().collect();
        let live = list
            .epoch_steward(0, |c| after.iter().any(|m| m == c))
            .unwrap();
        assert_ne!(live, nominal.as_slice());
        assert!(after.iter().any(|m| m == live));
    }

    #[test]
    fn test_epoch_and_backup_all_ineligible_and_single_survivor() {
        let config = StewardListConfig::new(2, 2).unwrap();
        let mems = members(&[1, 2]);
        let list = StewardList::generate(0, b"conversation", &mems, 2, config, 0).unwrap();

        let (e, b) = list.epoch_and_backup(0, |_| false);
        assert!(e.is_none() && b.is_none());

        let survivor = mems[0].clone();
        let (e, b) = list.epoch_and_backup(0, |c| c == survivor.as_slice());
        assert_eq!(e.unwrap(), survivor.as_slice());
        assert!(b.is_none());
    }

    #[test]
    fn test_epoch_and_backup_rotates_when_epoch_leaves() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let list = StewardList::generate(0, b"conversation", &mems, 3, config, 0).unwrap();

        let nominal = list.epoch_steward(0, |_| true).unwrap().to_vec();
        let (e, b) = list.epoch_and_backup(0, |c| c != nominal.as_slice());
        assert!(e.is_some() && b.is_some());
        assert_ne!(e.unwrap(), b.unwrap());
        assert_ne!(e.unwrap(), nominal.as_slice());
        assert_ne!(b.unwrap(), nominal.as_slice());
    }

    #[test]
    fn test_epoch_and_backup_matches_nominal_when_all_eligible() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let list = StewardList::generate(0, b"conversation", &mems, 3, config, 0).unwrap();

        let (e, b) = list.epoch_and_backup(0, |_| true);
        assert_eq!(e, list.epoch_steward(0, |_| true));
        assert_eq!(b, list.epoch_steward(1, |_| true));
    }

    #[test]
    fn test_sha256_sorting_is_ascending() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"conversation", &mems, 5, config, 0).unwrap();
        let hashes: Vec<Vec<u8>> = list
            .members()
            .iter()
            .map(|m| compute_steward_hash(0, 0, m, b"conversation"))
            .collect();

        for window in hashes.windows(2) {
            assert!(window[0] < window[1], "hashes must be ascending");
        }
    }

    #[test]
    fn test_validate_rejects_empty_list() {
        let config = StewardListConfig::new(3, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let empty: Vec<Vec<u8>> = vec![];

        assert!(StewardList::validate(&empty, 0, b"conversation", &mems, &config, 0).is_err());
    }

    #[test]
    fn test_below_sn_min_uses_all_members() {
        let config = StewardListConfig::new(5, 10).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"conversation", &mems, 3, config, 0).unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn test_large_conversation_subset_selection() {
        let config = StewardListConfig::new(3, 5).unwrap();
        let mems: Vec<Vec<u8>> = (1..=20).map(member).collect();

        let list = StewardList::generate(0, b"conversation", &mems, 5, config, 0).unwrap();
        assert_eq!(list.len(), 5);
        for steward in list.members() {
            assert!(mems.contains(steward));
        }
    }

    #[test]
    fn test_retry_rounds_produce_different_lists() {
        let config = StewardListConfig::new(1, 2).unwrap();
        let mems: Vec<Vec<u8>> = (1..=4u8).map(member).collect();

        let base = StewardList::generate(1, b"conversation", &mems, 2, config.clone(), 0).unwrap();
        let any_diff = (1..10u32).any(|r| {
            let other =
                StewardList::generate(1, b"conversation", &mems, 2, config.clone(), r).unwrap();
            other.members() != base.members()
        });
        assert!(
            any_diff,
            "retries should produce at least one different list"
        );
    }

    #[test]
    fn retry_round_seed_persists_independently_of_caller_counter() {
        let config = StewardListConfig::new(2, 4).unwrap();
        let mems: Vec<Vec<u8>> = (1..=4u8).map(member).collect();
        let epoch = 7;
        let accepted_round: u32 = 2;

        let list = StewardList::generate(epoch, b"conv", &mems, 4, config.clone(), accepted_round)
            .unwrap();
        assert_eq!(list.retry_round(), accepted_round, "list keeps its seed");

        let round0 = StewardList::generate(epoch, b"conv", &mems, 4, config.clone(), 0).unwrap();
        assert_ne!(
            list.members(),
            round0.members(),
            "retry_round must shuffle the ordering for this test to be meaningful"
        );

        assert!(
            StewardList::validate(
                list.members(),
                epoch,
                b"conv",
                list.members(),
                &config,
                accepted_round,
            )
            .unwrap(),
            "validate succeeds when the seed matches the list's recorded retry_round"
        );

        assert!(
            !StewardList::validate(list.members(), epoch, b"conv", list.members(), &config, 0,)
                .unwrap(),
            "validate fails when the seed differs — caller's counter is not the source of truth"
        );
    }
}
