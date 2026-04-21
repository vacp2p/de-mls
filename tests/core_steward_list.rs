use de_mls::core::{ProtocolConfig, StewardList};

fn member(id: u8) -> Vec<u8> {
    vec![id; 20]
}

fn members(ids: &[u8]) -> Vec<Vec<u8>> {
    ids.iter().map(|&id| member(id)).collect()
}

const GROUP: &[u8] = b"test-group";

// ── Deterministic generation ──────────────────────────────────────

#[test]
fn deterministic_same_inputs_same_output() {
    let config = ProtocolConfig::new(3, 5).unwrap();
    let mems = members(&[1, 2, 3, 4, 5]);

    let list1 = StewardList::generate(0, GROUP, &mems, 3, config.clone()).unwrap();
    let list2 = StewardList::generate(0, GROUP, &mems, 3, config).unwrap();

    assert_eq!(list1.members(), list2.members());
}

#[test]
fn input_order_does_not_matter() {
    let config = ProtocolConfig::new(3, 5).unwrap();

    let list_a =
        StewardList::generate(0, GROUP, &members(&[1, 2, 3, 4, 5]), 3, config.clone()).unwrap();
    let list_b = StewardList::generate(0, GROUP, &members(&[5, 3, 1, 4, 2]), 3, config).unwrap();

    assert_eq!(list_a.members(), list_b.members());
}

#[test]
fn different_epoch_shuffles_order() {
    // With all 10 members selected, the set is identical but the ordering
    // must differ for at least one pair of epochs out of 10 trials.
    let config = ProtocolConfig::new(10, 10).unwrap();
    let mems: Vec<Vec<u8>> = (1..=10).map(member).collect();

    let base = StewardList::generate(0, GROUP, &mems, 10, config.clone()).unwrap();
    let any_different = (1..10).any(|epoch| {
        let other = StewardList::generate(epoch, GROUP, &mems, 10, config.clone()).unwrap();
        other.members() != base.members()
    });
    assert!(
        any_different,
        "at least one epoch must produce a different ordering"
    );
}

#[test]
fn different_group_shuffles_order() {
    // Same logic: full selection, at least one of several group IDs must differ.
    let config = ProtocolConfig::new(10, 10).unwrap();
    let mems: Vec<Vec<u8>> = (1..=10).map(member).collect();

    let base = StewardList::generate(0, b"group-0", &mems, 10, config.clone()).unwrap();
    let any_different = (1..10).any(|i| {
        let gid = format!("group-{i}");
        let other = StewardList::generate(0, gid.as_bytes(), &mems, 10, config.clone()).unwrap();
        other.members() != base.members()
    });
    assert!(
        any_different,
        "at least one group must produce a different ordering"
    );
}

// ── Epoch steward and backup rotation ─────────────────────────────

#[test]
fn epoch_steward_rotates_through_all_members() {
    let config = ProtocolConfig::new(3, 3).unwrap();
    let mems = members(&[1, 2, 3]);

    let list = StewardList::generate(0, GROUP, &mems, 3, config).unwrap();

    let s0 = list.epoch_steward(0).unwrap().to_vec();
    let s1 = list.epoch_steward(1).unwrap().to_vec();
    let s2 = list.epoch_steward(2).unwrap().to_vec();

    // All three are different
    assert_ne!(s0, s1);
    assert_ne!(s1, s2);
    assert_ne!(s0, s2);

    // All three are from the members list
    assert!(list.contains(&s0));
    assert!(list.contains(&s1));
    assert!(list.contains(&s2));
}

#[test]
fn backup_steward_is_next_epoch_steward() {
    let config = ProtocolConfig::new(3, 3).unwrap();
    let mems = members(&[1, 2, 3]);

    let list = StewardList::generate(0, GROUP, &mems, 3, config).unwrap();

    assert_eq!(
        list.backup_steward(0).unwrap(),
        list.epoch_steward(1).unwrap()
    );
    assert_eq!(
        list.backup_steward(1).unwrap(),
        list.epoch_steward(2).unwrap()
    );
    // Backup wraps around
    assert_eq!(
        list.backup_steward(2).unwrap(),
        list.epoch_steward(0).unwrap()
    );
}

#[test]
fn start_epoch_offset() {
    let config = ProtocolConfig::new(2, 3).unwrap();
    let mems = members(&[1, 2, 3]);

    // List starts at epoch 10
    let list = StewardList::generate(10, GROUP, &mems, 3, config).unwrap();

    assert_eq!(list.start_epoch(), 10);

    // Epochs before start are exhausted
    assert!(list.is_exhausted(9));

    // Epochs 10-12 are valid
    assert!(list.epoch_steward(10).is_some());
    assert!(list.epoch_steward(11).is_some());
    assert!(list.epoch_steward(12).is_some());

    // Epoch 13 is exhausted
    assert!(list.is_exhausted(13));
    assert!(list.epoch_steward(13).is_none());
}

// ── List exhaustion ───────────────────────────────────────────────

#[test]
fn exhaustion_detection() {
    let config = ProtocolConfig::new(2, 2).unwrap();
    let mems = members(&[1, 2]);

    let list = StewardList::generate(0, GROUP, &mems, 2, config).unwrap();

    assert!(!list.is_exhausted(0));
    assert!(!list.is_exhausted(1));
    assert!(list.is_exhausted(2));
}

#[test]
fn exhausted_epoch_returns_none() {
    let config = ProtocolConfig::new(2, 2).unwrap();
    let mems = members(&[1, 2]);

    let list = StewardList::generate(0, GROUP, &mems, 2, config).unwrap();

    assert!(list.epoch_steward(2).is_none());
    assert!(list.backup_steward(2).is_none());
}

// ── Validation ────────────────────────────────────────────────────

#[test]
fn validate_correct_list() {
    let config = ProtocolConfig::new(3, 5).unwrap();
    let mems = members(&[1, 2, 3, 4, 5]);

    let list = StewardList::generate(0, GROUP, &mems, 3, config.clone()).unwrap();
    assert!(StewardList::validate(list.members(), 0, GROUP, &mems, &config).is_ok());
}

#[test]
fn validate_rejects_tampered_order() {
    let config = ProtocolConfig::new(3, 5).unwrap();
    let mems = members(&[1, 2, 3, 4, 5]);

    let list = StewardList::generate(0, GROUP, &mems, 3, config.clone()).unwrap();
    let mut tampered = list.members().to_vec();
    tampered.swap(0, 1);

    let valid = StewardList::validate(&tampered, 0, GROUP, &mems, &config);
    assert!(valid.is_ok());
    assert!(!valid.unwrap())
}

#[test]
fn validate_rejects_wrong_epoch() {
    // Use full selection (all 10 members) so the ordering is epoch-sensitive.
    let config = ProtocolConfig::new(10, 10).unwrap();
    let mems: Vec<Vec<u8>> = (1..=10).map(member).collect();

    let list = StewardList::generate(0, GROUP, &mems, 10, config.clone()).unwrap();

    // Find an epoch whose ordering differs from epoch 0
    let mismatched_epoch = (1..100u64)
        .find(|&e| {
            let other = StewardList::generate(e, GROUP, &mems, 10, config.clone()).unwrap();
            other.members() != list.members()
        })
        .expect("should find a different ordering within 100 epochs");

    let valid = StewardList::validate(list.members(), mismatched_epoch, GROUP, &mems, &config);
    assert!(valid.is_ok());
    assert!(!valid.unwrap())
}

#[test]
fn validate_rejects_substituted_member() {
    let config = ProtocolConfig::new(3, 5).unwrap();
    let mems = members(&[1, 2, 3, 4, 5]);

    let list = StewardList::generate(0, GROUP, &mems, 3, config.clone()).unwrap();
    let mut substituted = list.members().to_vec();
    // Replace first member with someone not in the original generation
    substituted[0] = member(99);

    let valid = StewardList::validate(&substituted, 0, GROUP, &mems, &config);
    assert!(valid.is_ok());
    assert!(!valid.unwrap())
}

#[test]
fn validate_rejects_empty_list() {
    let config = ProtocolConfig::new(3, 5).unwrap();
    let mems = members(&[1, 2, 3, 4, 5]);
    let empty: Vec<Vec<u8>> = vec![];

    assert!(StewardList::validate(&empty, 0, GROUP, &mems, &config).is_err());
}

// ── Edge cases ────────────────────────────────────────────────────

#[test]
fn single_member_group() {
    let config = ProtocolConfig::new(1, 3).unwrap();
    let mems = members(&[1]);

    let list = StewardList::generate(0, GROUP, &mems, 1, config).unwrap();

    assert_eq!(list.len(), 1);
    assert_eq!(
        list.epoch_steward(0).unwrap(),
        list.backup_steward(0).unwrap()
    );
    assert!(list.is_exhausted(1));
}

#[test]
fn below_sn_min_uses_all_members() {
    // sn_min=5 but only 3 members
    let config = ProtocolConfig::new(5, 10).unwrap();
    let mems = members(&[1, 2, 3]);

    let list = StewardList::generate(0, GROUP, &mems, 3, config).unwrap();
    assert_eq!(list.len(), 3);
}

#[test]
fn generate_returns_none_for_empty_members() {
    let config = ProtocolConfig::new(1, 3).unwrap();
    assert!(StewardList::generate(0, GROUP, &[], 1, config).is_err());
}

#[test]
fn large_group_subset_selection() {
    let config = ProtocolConfig::new(3, 5).unwrap();
    // 20 members, select top 5
    let mems: Vec<Vec<u8>> = (1..=20).map(member).collect();

    let list = StewardList::generate(0, GROUP, &mems, 5, config).unwrap();
    assert_eq!(list.len(), 5);

    // All selected members should be from the input
    for steward in list.members() {
        assert!(mems.contains(steward));
    }
}
