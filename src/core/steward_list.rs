//! Deterministic steward list — who commits each epoch, derived identically
//! on every member. See [`StewardList::generate`] for the hash-sort algorithm
//! and [`StewardList::epoch_steward`] / [`StewardList::backup_steward`] for
//! rotation. When `epoch >= start_epoch + len` the list is exhausted and a
//! new election MUST follow (RFC §Steward list creation).

use sha2::{Digest, Sha256};

use crate::core::CoreError;

/// Protocol configuration set at group creation.
///
/// Contains steward list size bounds and protocol-level flags that govern
/// group behavior. Shared across core and app layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolConfig {
    /// Minimum steward list size. If total members < sn_min, list size = total members.
    pub sn_min: usize,
    /// Maximum steward list size.
    pub sn_max: usize,
    /// Whether subset commit candidates are allowed during deterministic selection.
    pub allow_subset_candidates: bool,
}

impl ProtocolConfig {
    /// Create a new config with the given bounds.
    ///
    /// Returns `Err` if `sn_min` is 0 or `sn_min > sn_max`.
    /// Sets `allow_subset_candidates` to `false` by default.
    pub fn new(sn_min: usize, sn_max: usize) -> Result<Self, CoreError> {
        if sn_min < 1 || sn_min > sn_max {
            return Err(CoreError::InvalidConfigSize);
        }
        Ok(Self {
            sn_min,
            sn_max,
            allow_subset_candidates: false,
        })
    }

    /// Inclusive range of valid list sizes for `total_members` (RFC §Steward
    /// list creation). When `total_members < sn_min` the only valid size is
    /// `total_members`; otherwise the range is `[sn_min, min(sn_max, total)]`.
    fn size_bounds(&self, total_members: usize) -> std::ops::RangeInclusive<usize> {
        if total_members < self.sn_min {
            total_members..=total_members
        } else {
            self.sn_min..=self.sn_max.min(total_members)
        }
    }

    /// Preferred list size (the upper end of the valid range).
    pub fn compute_list_size(&self, total_members: usize) -> usize {
        *self.size_bounds(total_members).end()
    }

    /// `true` iff `size` lies within the valid range for this config.
    pub fn is_valid_size(&self, size: usize, total_members: usize) -> bool {
        self.size_bounds(total_members).contains(&size)
    }
}

/// An ordered list of steward identities for a range of epochs.
///
/// Generated deterministically so all group members arrive at the same list.
/// The list covers epochs `[start_epoch, start_epoch + len)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StewardList {
    /// Ordered steward identities (sorted by deterministic hash).
    members: Vec<Vec<u8>>,
    /// Configuration bounds.
    config: ProtocolConfig,
    /// The epoch at which this steward list became active.
    start_epoch: u64,
    /// The retry-round seed fed into the SHA256 sort that produced this list.
    /// Historical tag — frozen once the list is accepted. Distinct from
    /// `Group::reelection_round`, which is the dynamic counter for the
    /// *next* election attempt.
    retry_round: u32,
}

impl StewardList {
    /// Generate the deterministic steward list. Sorts candidates by
    /// `SHA256(epoch || retry_round || member_id || group_id)` and takes the
    /// first `sn`. Errors on empty `member_ids` or `sn` outside the config bounds.
    pub fn generate(
        election_epoch: u64,
        group_id: &[u8],
        member_ids: &[Vec<u8>],
        sn: usize,
        config: ProtocolConfig,
        retry_round: u32,
    ) -> Result<Self, CoreError> {
        check_generation_inputs(&config, member_ids, sn)?;
        let ordered = sorted_steward_indices(election_epoch, retry_round, group_id, member_ids);
        let members = ordered
            .into_iter()
            .take(sn)
            .map(|i| member_ids[i].clone())
            .collect();
        Ok(Self {
            members,
            config,
            start_epoch: election_epoch,
            retry_round,
        })
    }

    /// True iff `proposed` equals what [`Self::generate`] would produce for
    /// the same parameters. Compares in place — does not allocate a full
    /// [`StewardList`].
    pub fn validate(
        proposed: &[Vec<u8>],
        election_epoch: u64,
        group_id: &[u8],
        member_ids: &[Vec<u8>],
        config: &ProtocolConfig,
        retry_round: u32,
    ) -> Result<bool, CoreError> {
        let sn = proposed.len();
        check_generation_inputs(config, member_ids, sn)?;
        let ordered = sorted_steward_indices(election_epoch, retry_round, group_id, member_ids);
        Ok(ordered
            .iter()
            .take(sn)
            .zip(proposed.iter())
            .all(|(&i, want)| &member_ids[i] == want))
    }

    /// Nominal epoch steward at index `(epoch - start_epoch) % len`. Use
    /// [`Self::live_epoch_steward`] to skip stewards no longer in the group.
    pub fn epoch_steward(&self, epoch: u64) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let index = ((epoch - self.start_epoch) as usize) % self.members.len();
        Some(&self.members[index])
    }

    /// Nominal backup steward at index `(epoch - start_epoch + 1) % len`.
    pub fn backup_steward(&self, epoch: u64) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let index = ((epoch - self.start_epoch) as usize + 1) % self.members.len();
        Some(&self.members[index])
    }

    /// Deterministic proposer of the next election when this list exhausts.
    /// Walks rotation from index 0; returns `None` if no steward is eligible.
    pub fn responsible_election_proposer<F: Fn(&[u8]) -> bool>(
        &self,
        eligible: F,
    ) -> Option<&[u8]> {
        // Passing `start_epoch` makes live_epoch_steward start at index 0.
        self.live_epoch_steward(self.start_epoch, eligible)
    }

    /// Epoch steward with the nominal rotation advanced past any steward for
    /// whom `eligible` returns `false` (typically: removed members or those
    /// pending a self-leave).
    pub fn live_epoch_steward<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        eligible: F,
    ) -> Option<&[u8]> {
        self.live_steward_from(epoch, 0, eligible)
    }

    /// Live epoch steward + a distinct backup. Resolving them together
    /// stops the epoch-steward walk from landing on the nominal backup
    /// and collapsing both roles onto the same identity. Backup is `None`
    /// when fewer than two stewards are eligible.
    pub fn live_epoch_and_backup<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        eligible: F,
    ) -> (Option<&[u8]>, Option<&[u8]>) {
        let epoch_steward = self.live_steward_from(epoch, 0, &eligible);
        let backup = epoch_steward
            .and_then(|es| self.live_steward_from(epoch, 1, |c| c != es && eligible(c)));
        (epoch_steward, backup)
    }

    fn live_steward_from<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        offset: usize,
        eligible: F,
    ) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let len = self.members.len();
        let start = ((epoch - self.start_epoch) as usize + offset) % len;
        for step in 0..len {
            let idx = (start + step) % len;
            let candidate = &self.members[idx];
            if eligible(candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// `true` once every steward has served — the list covers
    /// `[start_epoch, start_epoch + len)`. A new election MUST follow.
    pub fn is_exhausted(&self, epoch: u64) -> bool {
        if epoch < self.start_epoch {
            return true;
        }
        (epoch - self.start_epoch) >= self.members.len() as u64
    }

    pub fn contains(&self, member_id: &[u8]) -> bool {
        self.members.iter().any(|m| m.as_slice() == member_id)
    }

    pub fn members(&self) -> &[Vec<u8>] {
        &self.members
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn config(&self) -> &ProtocolConfig {
        &self.config
    }

    pub fn start_epoch(&self) -> u64 {
        self.start_epoch
    }

    /// Historical tag — the retry-round seed that was fed into the SHA256
    /// sort when this list was accepted. Joiners carry this value in
    /// `GroupSync` so they can re-derive the same ordering.
    pub fn retry_round(&self) -> u32 {
        self.retry_round
    }
}

/// Shared precondition check for [`StewardList::generate`] and [`StewardList::validate`].
fn check_generation_inputs(
    config: &ProtocolConfig,
    member_ids: &[Vec<u8>],
    sn: usize,
) -> Result<(), CoreError> {
    if member_ids.is_empty() {
        return Err(CoreError::EmptyMembersList);
    }
    if !config.is_valid_size(sn, member_ids.len()) {
        return Err(CoreError::InvalidConfigSize);
    }
    Ok(())
}

/// Return candidate member indices sorted ascending by their steward hash.
/// Kept index-based so callers decide whether to clone or borrow.
fn sorted_steward_indices(
    election_epoch: u64,
    retry_round: u32,
    group_id: &[u8],
    member_ids: &[Vec<u8>],
) -> Vec<usize> {
    let mut scored: Vec<(Vec<u8>, usize)> = member_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                compute_steward_hash(election_epoch, retry_round, id, group_id),
                i,
            )
        })
        .collect();
    scored.sort_by(|(a, _), (b, _)| a.cmp(b));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// `SHA256(epoch || retry_round || member_id || group_id)`, big-endian for
/// the integers. `retry_round` is mixed in so successive election retries
/// within one MLS epoch propose different list compositions.
fn compute_steward_hash(
    epoch: u64,
    retry_round: u32,
    member_id: &[u8],
    group_id: &[u8],
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(epoch.to_be_bytes());
    hasher.update(retry_round.to_be_bytes());
    hasher.update(member_id);
    hasher.update(group_id);
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20] // 20-byte member id
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    #[test]
    fn test_config_validation() {
        let config = ProtocolConfig::new(2, 5).unwrap();

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
        assert!(ProtocolConfig::new(0, 5).is_err(), "sn_min == 0");
        assert!(ProtocolConfig::new(5, 3).is_err(), "sn_min > sn_max");
    }

    #[test]
    fn test_generate_empty_members() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        assert!(StewardList::generate(0, b"group1", &[], 1, config, 0).is_err());
    }

    #[test]
    fn test_generate_invalid_sn() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        assert!(
            StewardList::generate(0, b"group1", &mems, 1, config.clone(), 0).is_err(),
            "below sn_min"
        );
        assert!(
            StewardList::generate(0, b"group1", &mems, 6, config, 0).is_err(),
            "above sn_max"
        );
    }

    #[test]
    fn test_deterministic_generation() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let group_id = b"test-group";

        let list1 = StewardList::generate(0, group_id, &mems, 3, config.clone(), 0).unwrap();
        let list2 = StewardList::generate(0, group_id, &mems, 3, config, 0).unwrap();

        assert_eq!(list1.members(), list2.members());
        assert_eq!(list1.len(), 3);
    }

    /// With the full candidate set and only the epoch differing, the order
    /// must shuffle for at least one epoch in a small window.
    #[test]
    fn test_different_epoch_shuffles() {
        let config = ProtocolConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let base = StewardList::generate(0, b"group", &mems, 5, config.clone(), 0).unwrap();
        let any_diff = (1..10).any(|e| {
            let other = StewardList::generate(e, b"group", &mems, 5, config.clone(), 0).unwrap();
            other.members() != base.members()
        });
        assert!(any_diff);
    }

    #[test]
    fn test_different_group_shuffles() {
        let config = ProtocolConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let base = StewardList::generate(0, b"group1", &mems, 5, config.clone(), 0).unwrap();
        let other = StewardList::generate(0, b"group2", &mems, 5, config, 0).unwrap();
        assert_ne!(base.members(), other.members());
    }

    #[test]
    fn test_member_order_does_not_affect_result() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mems_a = members(&[1, 2, 3, 4, 5]);
        let mems_b = members(&[5, 3, 1, 4, 2]);

        let list_a = StewardList::generate(0, b"group", &mems_a, 3, config.clone(), 0).unwrap();
        let list_b = StewardList::generate(0, b"group", &mems_b, 3, config, 0).unwrap();

        assert_eq!(list_a.members(), list_b.members());
    }

    #[test]
    fn test_epoch_steward_rotation() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();
        let s0 = list.epoch_steward(0).unwrap().to_vec();
        let s1 = list.epoch_steward(1).unwrap().to_vec();
        let s2 = list.epoch_steward(2).unwrap().to_vec();

        assert_ne!(s0, s1);
        assert_ne!(s1, s2);
        assert_ne!(s0, s2);
    }

    /// Backup at epoch `e` is the epoch steward at `e + 1` (mod len).
    #[test]
    fn test_backup_steward() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();

        assert_eq!(list.backup_steward(0), list.epoch_steward(1));
        assert_eq!(list.backup_steward(1), list.epoch_steward(2));
        assert_eq!(list.backup_steward(2), list.epoch_steward(0));
    }

    #[test]
    fn test_list_exhaustion() {
        let config = ProtocolConfig::new(2, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(5, b"group", &mems, 3, config, 0).unwrap();
        assert_eq!(list.start_epoch(), 5);

        // Covered epochs: [5, 8)
        assert!(!list.is_exhausted(5));
        assert!(!list.is_exhausted(7));
        assert!(list.is_exhausted(8));
        assert!(list.is_exhausted(4), "before start_epoch is exhausted");

        // Exhausted epochs return None from both rotation slots.
        assert!(list.epoch_steward(8).is_none());
        assert!(list.backup_steward(8).is_none());
    }

    #[test]
    fn test_validate_correct_list() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 3, config.clone(), 0).unwrap();
        let valid = StewardList::validate(list.members(), 0, b"group", &mems, &config, 0);
        assert!(valid.is_ok());
        assert!(valid.unwrap())
    }

    #[test]
    fn test_validate_tampered_list() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let mut list = StewardList::generate(0, b"group", &mems, 3, config.clone(), 0).unwrap();
        // Swap first two members
        list.members.swap(0, 1);

        let valid = StewardList::validate(list.members(), 0, b"group", &mems, &config, 0);
        assert!(valid.is_ok());
        assert!(!valid.unwrap())
    }

    #[test]
    fn test_validate_wrong_epoch() {
        let config = ProtocolConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 5, config.clone(), 0).unwrap();
        // Find an epoch that produces a different ordering
        let diff_epoch = (1..100u64)
            .find(|&e| {
                let o = StewardList::generate(e, b"group", &mems, 5, config.clone(), 0).unwrap();
                o.members() != list.members()
            })
            .expect("should differ within 100 epochs");

        let valid = StewardList::validate(list.members(), diff_epoch, b"group", &mems, &config, 0);
        assert!(valid.is_ok());
        assert!(!valid.unwrap());
    }

    /// `sn == total_members` so any change in the candidate set forces a
    /// different output ordering.
    #[test]
    fn test_validate_wrong_members() {
        let config = ProtocolConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let other_mems = members(&[1, 2, 3, 4, 6]);

        let list = StewardList::generate(0, b"group", &mems, 5, config.clone(), 0).unwrap();
        let valid = StewardList::validate(list.members(), 0, b"group", &other_mems, &config, 0);
        assert!(valid.is_ok());
        assert!(!valid.unwrap())
    }

    #[test]
    fn test_single_member() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let mems = members(&[1]);

        let list = StewardList::generate(0, b"group", &mems, 1, config, 0).unwrap();
        assert_eq!(list.len(), 1);
        // With one steward, epoch and backup slots collapse to the same person.
        assert_eq!(list.epoch_steward(0), list.backup_steward(0));
        assert!(list.is_exhausted(1));
    }

    /// With everyone eligible, live == nominal. With the nominal filtered out,
    /// live rotates to the next eligible steward.
    #[test]
    fn test_live_epoch_steward_skips_removed() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();
        let nominal = list.epoch_steward(0).unwrap().to_vec();

        let all_eligible = |c: &[u8]| mems.iter().any(|m| m == c);
        assert_eq!(
            list.live_epoch_steward(0, all_eligible),
            Some(nominal.as_slice())
        );

        let after: Vec<Vec<u8>> = mems.iter().filter(|m| **m != nominal).cloned().collect();
        let live = list
            .live_epoch_steward(0, |c| after.iter().any(|m| m == c))
            .unwrap();
        assert_ne!(live, nominal.as_slice());
        assert!(after.iter().any(|m| m == live));
    }

    /// All stewards ineligible → both slots are None. One eligible → epoch
    /// resolves, backup stays None (can't be distinct from epoch).
    #[test]
    fn test_live_epoch_and_backup_all_ineligible_and_single_survivor() {
        let config = ProtocolConfig::new(2, 2).unwrap();
        let mems = members(&[1, 2]);
        let list = StewardList::generate(0, b"group", &mems, 2, config, 0).unwrap();

        let (e, b) = list.live_epoch_and_backup(0, |_| false);
        assert!(e.is_none() && b.is_none());

        let survivor = mems[0].clone();
        let (e, b) = list.live_epoch_and_backup(0, |c| c == survivor.as_slice());
        assert_eq!(e.unwrap(), survivor.as_slice());
        assert!(b.is_none());
    }

    /// 3 stewards with the nominal epoch steward leaving: both slots must
    /// rotate and stay distinct. The pre-fix path would collapse them.
    #[test]
    fn test_live_epoch_and_backup_rotates_when_epoch_leaves() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();

        let nominal = list.epoch_steward(0).unwrap().to_vec();
        let (e, b) = list.live_epoch_and_backup(0, |c| c != nominal.as_slice());
        assert!(e.is_some() && b.is_some());
        assert_ne!(e.unwrap(), b.unwrap());
        assert_ne!(e.unwrap(), nominal.as_slice());
        assert_ne!(b.unwrap(), nominal.as_slice());
    }

    /// Happy path (no leavers) → matches the nominal `epoch_steward` /
    /// `backup_steward` assignment.
    #[test]
    fn test_live_epoch_and_backup_matches_nominal_when_all_eligible() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();

        let (e, b) = list.live_epoch_and_backup(0, |_| true);
        assert_eq!(e, list.epoch_steward(0));
        assert_eq!(b, list.backup_steward(0));
    }

    #[test]
    fn test_sha256_sorting_is_ascending() {
        let config = ProtocolConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 5, config, 0).unwrap();
        let hashes: Vec<Vec<u8>> = list
            .members()
            .iter()
            .map(|m| compute_steward_hash(0, 0, m, b"group"))
            .collect();

        for window in hashes.windows(2) {
            assert!(window[0] < window[1], "hashes must be ascending");
        }
    }

    #[test]
    fn test_validate_rejects_empty_list() {
        let config = ProtocolConfig::new(3, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let empty: Vec<Vec<u8>> = vec![];

        assert!(StewardList::validate(&empty, 0, b"group", &mems, &config, 0).is_err());
    }

    /// `sn_min=5` but only 3 members: `generate` clamps to total available.
    #[test]
    fn test_below_sn_min_uses_all_members() {
        let config = ProtocolConfig::new(5, 10).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn test_large_group_subset_selection() {
        let config = ProtocolConfig::new(3, 5).unwrap();
        let mems: Vec<Vec<u8>> = (1..=20).map(member).collect();

        let list = StewardList::generate(0, b"group", &mems, 5, config, 0).unwrap();
        assert_eq!(list.len(), 5);
        for steward in list.members() {
            assert!(mems.contains(steward));
        }
    }

    /// With 4 members and sn=2, different retry_round values MUST produce at
    /// least some different orderings. Otherwise the retry mechanism is broken.
    #[test]
    fn test_retry_rounds_produce_different_lists() {
        let config = ProtocolConfig::new(1, 2).unwrap();
        let mems: Vec<Vec<u8>> = (1..=4u8).map(member).collect();

        let base = StewardList::generate(1, b"group", &mems, 2, config.clone(), 0).unwrap();
        let any_diff = (1..10u32).any(|r| {
            let other = StewardList::generate(1, b"group", &mems, 2, config.clone(), r).unwrap();
            other.members() != base.members()
        });
        assert!(
            any_diff,
            "retries should produce at least one different list"
        );
    }
}
