//! Deterministic steward list generation and rotation.
//!
//! The steward list is an ordered list of member identities that determines
//! who creates commits for each epoch. All members independently derive the
//! same list using `SHA256(epoch || member_id || group_id)` sorting.
//!
//! # Generation Algorithm (RFC §Steward list creation)
//!
//! 1. For each candidate member, compute `SHA256(epoch || member_id || group_id)`
//! 2. Sort candidates in ascending order by hash value
//! 3. Take the first `sn` members (`sn_min ≤ sn ≤ sn_max`)
//!
//! # Rotation
//!
//! - **Epoch steward**: steward at index `epoch % list_len`
//! - **Backup steward**: steward at index `(epoch + 1) % list_len`
//!
//! When all stewards have served (epoch reaches `start_epoch + list_len`),
//! the list is exhausted and a new election MUST be initiated.

use sha2::{Digest, Sha256};

/// Configuration for steward list size bounds, set at group creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StewardListConfig {
    /// Minimum steward list size. If total members < sn_min, list size = total members.
    pub sn_min: usize,
    /// Maximum steward list size.
    pub sn_max: usize,
}

impl StewardListConfig {
    /// Create a new config with the given bounds.
    ///
    /// # Panics
    ///
    /// Panics if `sn_min` is 0 or `sn_min > sn_max`.
    pub fn new(sn_min: usize, sn_max: usize) -> Self {
        assert!(sn_min >= 1, "sn_min must be at least 1");
        assert!(sn_min <= sn_max, "sn_min must be <= sn_max");
        Self { sn_min, sn_max }
    }

    /// Check if a given list size is valid for this config and member count.
    pub fn is_valid_size(&self, size: usize, total_members: usize) -> bool {
        if total_members < self.sn_min {
            // RFC: if total members < sn_min, list size = total member count
            size == total_members
        } else {
            size >= self.sn_min && size <= self.sn_max && size <= total_members
        }
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
    config: StewardListConfig,
    /// The epoch at which this steward list became active.
    start_epoch: u64,
}

impl StewardList {
    /// Generate a deterministic steward list from the given members.
    ///
    /// Sorts candidates by `SHA256(epoch || member_id || group_id)` and takes
    /// the first `sn` entries. The `sn` parameter controls list size and must
    /// satisfy `sn_min ≤ sn ≤ sn_max` (or equal total members if below sn_min).
    ///
    /// Returns `None` if:
    /// - `member_ids` is empty
    /// - `sn` is not valid for the config and member count
    pub fn generate(
        election_epoch: u64,
        group_id: &[u8],
        member_ids: &[Vec<u8>],
        sn: usize,
        config: StewardListConfig,
    ) -> Option<Self> {
        if member_ids.is_empty() {
            return None;
        }

        if !config.is_valid_size(sn, member_ids.len()) {
            return None;
        }

        let mut scored: Vec<(Vec<u8>, Vec<u8>)> = member_ids
            .iter()
            .map(|member_id| {
                let hash = compute_steward_hash(election_epoch, member_id, group_id);
                (hash, member_id.clone())
            })
            .collect();

        // Sort ascending by hash value
        scored.sort_by(|(hash_a, _), (hash_b, _)| hash_a.cmp(hash_b));

        let members: Vec<Vec<u8>> = scored.into_iter().take(sn).map(|(_, id)| id).collect();

        Some(Self {
            members,
            config,
            start_epoch: election_epoch,
        })
    }

    /// Validate that a proposed steward list matches the deterministic generation.
    ///
    /// Returns `true` if `proposed` equals the list that `generate()` would produce
    /// with the same parameters.
    pub fn validate(
        proposed: &[Vec<u8>],
        election_epoch: u64,
        group_id: &[u8],
        member_ids: &[Vec<u8>],
        config: &StewardListConfig,
    ) -> bool {
        let sn = proposed.len();
        let Some(expected) =
            StewardList::generate(election_epoch, group_id, member_ids, sn, config.clone())
        else {
            return false;
        };
        expected.members == proposed
    }

    /// Get the epoch steward for the given epoch.
    ///
    /// The epoch steward is at index `(epoch - start_epoch) % len`.
    /// Returns `None` if the list is exhausted for this epoch.
    pub fn epoch_steward(&self, epoch: u64) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let index = ((epoch - self.start_epoch) as usize) % self.members.len();
        Some(&self.members[index])
    }

    /// Get the backup steward for the given epoch.
    ///
    /// The backup steward is at index `(epoch - start_epoch + 1) % len`.
    /// Returns `None` if the list is exhausted for this epoch.
    pub fn backup_steward(&self, epoch: u64) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let index = ((epoch - self.start_epoch) as usize + 1) % self.members.len();
        Some(&self.members[index])
    }

    /// Check if the list is exhausted at the given epoch.
    ///
    /// The list covers epochs `[start_epoch, start_epoch + len)`. Once all
    /// stewards have served, a new election MUST be initiated.
    pub fn is_exhausted(&self, epoch: u64) -> bool {
        if epoch < self.start_epoch {
            return true;
        }
        (epoch - self.start_epoch) >= self.members.len() as u64
    }

    /// Check if a member is in the steward list.
    pub fn contains(&self, member_id: &[u8]) -> bool {
        self.members.iter().any(|m| m.as_slice() == member_id)
    }

    /// Check if a member is the epoch steward for the given epoch.
    pub fn is_epoch_steward(&self, member_id: &[u8], epoch: u64) -> bool {
        self.epoch_steward(epoch).is_some_and(|s| s == member_id)
    }

    /// Get the ordered steward identities.
    pub fn members(&self) -> &[Vec<u8>] {
        &self.members
    }

    /// Get the number of stewards in the list.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Check if the list is empty.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Get the config.
    pub fn config(&self) -> &StewardListConfig {
        &self.config
    }

    /// Get the start epoch.
    pub fn start_epoch(&self) -> u64 {
        self.start_epoch
    }

    /// Get the last epoch this list covers (inclusive).
    pub fn last_epoch(&self) -> u64 {
        self.start_epoch + self.members.len() as u64 - 1
    }
}

/// Compute the deterministic hash for steward list sorting.
///
/// `SHA256(epoch || member_id || group_id)` where epoch is big-endian u64.
fn compute_steward_hash(epoch: u64, member_id: &[u8], group_id: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(epoch.to_be_bytes());
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
        let config = StewardListConfig::new(2, 5);

        // Normal range
        assert!(config.is_valid_size(3, 10));
        assert!(config.is_valid_size(2, 10));
        assert!(config.is_valid_size(5, 10));

        // Out of range
        assert!(!config.is_valid_size(1, 10));
        assert!(!config.is_valid_size(6, 10));

        // Below sn_min members: list size must equal total
        assert!(config.is_valid_size(1, 1));
        assert!(!config.is_valid_size(2, 1));
    }

    #[test]
    #[should_panic(expected = "sn_min must be at least 1")]
    fn test_config_zero_sn_min() {
        StewardListConfig::new(0, 5);
    }

    #[test]
    #[should_panic(expected = "sn_min must be <= sn_max")]
    fn test_config_min_exceeds_max() {
        StewardListConfig::new(5, 3);
    }

    #[test]
    fn test_generate_empty_members() {
        let config = StewardListConfig::new(1, 3);
        assert!(StewardList::generate(0, b"group1", &[], 1, config).is_none());
    }

    #[test]
    fn test_generate_invalid_sn() {
        let config = StewardListConfig::new(2, 5);
        let mems = members(&[1, 2, 3, 4, 5]);

        // sn below sn_min
        assert!(StewardList::generate(0, b"group1", &mems, 1, config.clone()).is_none());
        // sn above sn_max
        assert!(StewardList::generate(0, b"group1", &mems, 6, config).is_none());
    }

    #[test]
    fn test_deterministic_generation() {
        let config = StewardListConfig::new(2, 5);
        let mems = members(&[1, 2, 3, 4, 5]);
        let group_id = b"test-group";

        let list1 = StewardList::generate(0, group_id, &mems, 3, config.clone()).unwrap();
        let list2 = StewardList::generate(0, group_id, &mems, 3, config).unwrap();

        assert_eq!(list1.members(), list2.members());
        assert_eq!(list1.len(), 3);
    }

    #[test]
    fn test_different_epoch_shuffles() {
        // Use all members so the set is the same; order must differ for at least one epoch.
        let config = StewardListConfig::new(5, 5);
        let mems = members(&[1, 2, 3, 4, 5]);

        let base = StewardList::generate(0, b"group", &mems, 5, config.clone()).unwrap();
        let any_diff = (1..10).any(|e| {
            let other = StewardList::generate(e, b"group", &mems, 5, config.clone()).unwrap();
            other.members() != base.members()
        });
        assert!(any_diff);
    }

    #[test]
    fn test_different_group_shuffles() {
        let config = StewardListConfig::new(5, 5);
        let mems = members(&[1, 2, 3, 4, 5]);

        let base = StewardList::generate(0, b"group1", &mems, 5, config.clone()).unwrap();
        let other = StewardList::generate(0, b"group2", &mems, 5, config).unwrap();
        // With 5! = 120 possible orderings, collision is extremely unlikely
        assert_ne!(base.members(), other.members());
    }

    #[test]
    fn test_member_order_does_not_affect_result() {
        let config = StewardListConfig::new(2, 5);
        let mems_a = members(&[1, 2, 3, 4, 5]);
        let mems_b = members(&[5, 3, 1, 4, 2]);

        let list_a = StewardList::generate(0, b"group", &mems_a, 3, config.clone()).unwrap();
        let list_b = StewardList::generate(0, b"group", &mems_b, 3, config).unwrap();

        assert_eq!(list_a.members(), list_b.members());
    }

    #[test]
    fn test_epoch_steward_rotation() {
        let config = StewardListConfig::new(3, 3);
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config).unwrap();
        let s0 = list.epoch_steward(0).unwrap().to_vec();
        let s1 = list.epoch_steward(1).unwrap().to_vec();
        let s2 = list.epoch_steward(2).unwrap().to_vec();

        // All three stewards should be different (different members)
        assert_ne!(s0, s1);
        assert_ne!(s1, s2);
        assert_ne!(s0, s2);
    }

    #[test]
    fn test_backup_steward() {
        let config = StewardListConfig::new(3, 3);
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config).unwrap();

        // Backup for epoch 0 should be epoch steward for epoch 1
        assert_eq!(
            list.backup_steward(0).unwrap(),
            list.epoch_steward(1).unwrap()
        );
        // Backup for epoch 1 should be epoch steward for epoch 2
        assert_eq!(
            list.backup_steward(1).unwrap(),
            list.epoch_steward(2).unwrap()
        );
        // Backup for epoch 2 wraps around to epoch steward for epoch 0
        assert_eq!(
            list.backup_steward(2).unwrap(),
            list.epoch_steward(0).unwrap()
        );
    }

    #[test]
    fn test_list_exhaustion() {
        let config = StewardListConfig::new(2, 3);
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(5, b"group", &mems, 3, config).unwrap();
        assert_eq!(list.start_epoch(), 5);
        assert_eq!(list.last_epoch(), 7);

        // Epochs 5, 6, 7 are covered
        assert!(!list.is_exhausted(5));
        assert!(!list.is_exhausted(6));
        assert!(!list.is_exhausted(7));

        // Epoch 8 is exhausted
        assert!(list.is_exhausted(8));

        // Before start is also exhausted
        assert!(list.is_exhausted(4));
    }

    #[test]
    fn test_exhausted_returns_none() {
        let config = StewardListConfig::new(2, 2);
        let mems = members(&[1, 2]);

        let list = StewardList::generate(0, b"group", &mems, 2, config).unwrap();

        assert!(list.epoch_steward(2).is_none());
        assert!(list.backup_steward(2).is_none());
    }

    #[test]
    fn test_validate_correct_list() {
        let config = StewardListConfig::new(2, 5);
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 3, config.clone()).unwrap();
        assert!(StewardList::validate(
            list.members(),
            0,
            b"group",
            &mems,
            &config,
        ));
    }

    #[test]
    fn test_validate_tampered_list() {
        let config = StewardListConfig::new(2, 5);
        let mems = members(&[1, 2, 3, 4, 5]);

        let mut list = StewardList::generate(0, b"group", &mems, 3, config.clone()).unwrap();
        // Swap first two members
        list.members.swap(0, 1);

        assert!(!StewardList::validate(
            list.members(),
            0,
            b"group",
            &mems,
            &config,
        ));
    }

    #[test]
    fn test_validate_wrong_epoch() {
        let config = StewardListConfig::new(5, 5);
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 5, config.clone()).unwrap();
        // Find an epoch that produces a different ordering
        let diff_epoch = (1..100u64)
            .find(|&e| {
                let o = StewardList::generate(e, b"group", &mems, 5, config.clone()).unwrap();
                o.members() != list.members()
            })
            .expect("should differ within 100 epochs");

        assert!(!StewardList::validate(
            list.members(),
            diff_epoch,
            b"group",
            &mems,
            &config,
        ));
    }

    #[test]
    fn test_validate_wrong_members() {
        // Use full selection so any change in the candidate set changes the output.
        let config = StewardListConfig::new(5, 5);
        let mems = members(&[1, 2, 3, 4, 5]);
        let other_mems = members(&[1, 2, 3, 4, 6]); // different member 6

        let list = StewardList::generate(0, b"group", &mems, 5, config.clone()).unwrap();
        assert!(!StewardList::validate(
            list.members(),
            0,
            b"group",
            &other_mems,
            &config,
        ));
    }

    #[test]
    fn test_contains() {
        let config = StewardListConfig::new(2, 3);
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 3, config).unwrap();

        // Some of the 5 members should be in the list of 3
        let in_list: Vec<bool> = (1..=5).map(|id| list.contains(&member(id))).collect();
        assert_eq!(in_list.iter().filter(|&&b| b).count(), 3);
    }

    #[test]
    fn test_is_epoch_steward() {
        let config = StewardListConfig::new(2, 3);
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config).unwrap();
        let steward_0 = list.epoch_steward(0).unwrap().to_vec();

        assert!(list.is_epoch_steward(&steward_0, 0));
        // The epoch-0 steward is not necessarily the epoch-1 steward
        // (unless the list has only 1 member)
    }

    #[test]
    fn test_below_sn_min_uses_total_members() {
        // sn_min=3 but only 2 members — list size must be 2
        let config = StewardListConfig::new(3, 5);
        let mems = members(&[1, 2]);

        let list = StewardList::generate(0, b"group", &mems, 2, config).unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_single_member() {
        let config = StewardListConfig::new(1, 3);
        let mems = members(&[1]);

        let list = StewardList::generate(0, b"group", &mems, 1, config).unwrap();
        assert_eq!(list.len(), 1);

        // Same steward for every epoch in range
        assert_eq!(
            list.epoch_steward(0).unwrap(),
            list.backup_steward(0).unwrap()
        );
        assert!(list.is_exhausted(1));
    }

    #[test]
    fn test_sha256_sorting_is_ascending() {
        let config = StewardListConfig::new(5, 5);
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 5, config).unwrap();

        // Verify that the list is sorted by ascending hash
        let hashes: Vec<Vec<u8>> = list
            .members()
            .iter()
            .map(|m| compute_steward_hash(0, m, b"group"))
            .collect();

        for window in hashes.windows(2) {
            assert!(window[0] < window[1], "hashes must be in ascending order");
        }
    }
}
