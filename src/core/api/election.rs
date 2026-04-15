//! Steward election validation and application.

use super::*;

use crate::core::steward_list::StewardList;

// ─────────────────────────── Election Validation ───────────────────────────

/// Validate a proposed steward election list against the current group state.
///
/// The proposed list is valid if `StewardList::validate()` confirms it matches
/// the deterministic generation for the given epoch, group, and member set.
///
/// This is a pure function — app layer calls it before applying the election
/// result, since `apply_consensus_result` does not have access to the MLS
/// member list.
pub fn validate_election_proposal(
    group: &Group,
    proposed_stewards: &[Vec<u8>],
    election_epoch: u64,
    member_ids: &[Vec<u8>],
) -> Result<bool, CoreError> {
    StewardList::validate(
        proposed_stewards,
        election_epoch,
        group.group_name_bytes(),
        member_ids,
        group.protocol_config(),
    )
}

/// Apply a validated election result to the group.
///
/// Generates a new `StewardList` from the proposed stewards and sets it on the
/// group. Returns `true` if the list was successfully generated and applied,
/// `false` if generation failed (invalid parameters).
///
/// The caller MUST validate the proposal via [`validate_election_proposal`]
/// before calling this function.
pub fn apply_election_result(
    group: &mut Group,
    proposed_stewards: &[Vec<u8>],
    election_epoch: u64,
) -> Result<(), CoreError> {
    let sn = proposed_stewards.len();
    group.generate_and_set_steward_list(election_epoch, proposed_stewards, sn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::consensus::apply_consensus_result;
    use crate::core::steward_list::ProtocolConfig;
    use crate::protos::de_mls::messages::v1::{
        GroupUpdateRequest, StewardElectionProposal, group_update_request,
    };
    use prost::Message;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    #[test]
    fn test_validate_election_proposal_accepts_valid() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let group = Group::new_as_creator("test-group", member(1), config.clone()).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-group", &mems, sn, config).unwrap();

        let valid = validate_election_proposal(&group, list.members(), 10, &mems);
        assert!(valid.is_ok());
        assert!(valid.unwrap());
    }

    #[test]
    fn test_validate_election_proposal_rejects_tampered() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let group = Group::new_as_creator("test-group", member(1), config.clone()).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-group", &mems, sn, config).unwrap();
        let mut tampered = list.members().to_vec();
        tampered.swap(0, 1);

        let valid = validate_election_proposal(&group, &tampered, 10, &mems);
        assert!(valid.is_ok());
        assert!(!valid.unwrap());
    }

    #[test]
    fn test_validate_election_proposal_rejects_wrong_epoch() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let group = Group::new_as_creator("test-group", member(1), config.clone()).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-group", &mems, sn, config).unwrap();

        let valid = validate_election_proposal(&group, list.members(), 99, &mems);
        assert!(valid.is_ok());
        assert!(!valid.unwrap());
    }

    #[test]
    fn test_apply_election_result_sets_list() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config.clone()).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-group", &mems, sn, config).unwrap();

        assert!(apply_election_result(&mut group, list.members(), 10).is_ok());

        let new_list = group.steward_list().unwrap();
        assert_eq!(new_list.start_epoch(), 10);
        assert_eq!(new_list.len(), 5);
    }

    /// Test that `apply_consensus_result` handles election YES correctly:
    /// - Returns an `ElectionOutcome` in the result
    /// - Does not leave the election in the approved queue
    /// - No score ops
    #[test]
    fn test_consensus_apply_election_yes_owner() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config.clone()).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-group", &mems, sn, config).unwrap();

        let election_request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards: list.members().to_vec(),
                    election_epoch: 10,
                },
            )),
        };

        let proposal_id = 42;
        group.store_voting_proposal(proposal_id, election_request.clone());

        let payload = election_request.encode_to_vec();
        let result = apply_consensus_result(&mut group, proposal_id, true, &payload).unwrap();

        // Should have election outcome
        assert!(result.election.is_some());
        let outcome = result.election.unwrap();
        assert_eq!(outcome.election_epoch, 10);
        assert_eq!(outcome.proposed_stewards.len(), 5);

        // No score ops for elections
        assert!(result.score_ops.is_empty());

        // Proposal should NOT be in approved queue (no MLS operation)
        assert_eq!(group.approved_proposals_count(), 0);
    }

    /// Test that `apply_consensus_result` handles election NO correctly:
    /// - Returns empty result (no election, no score ops)
    /// - Proposal is removed from voting queue
    #[test]
    fn test_consensus_apply_election_no_owner() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();

        let election_request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards: vec![member(1), member(2)],
                    election_epoch: 10,
                },
            )),
        };

        let proposal_id = 43;
        group.store_voting_proposal(proposal_id, election_request.clone());

        let payload = election_request.encode_to_vec();
        let result = apply_consensus_result(&mut group, proposal_id, false, &payload).unwrap();

        // No election outcome on rejection
        assert!(result.election.is_none());
        assert!(result.score_ops.is_empty());
        assert_eq!(group.approved_proposals_count(), 0);
    }

    /// Test that election YES for non-owner returns the outcome but doesn't touch handle queues.
    #[test]
    fn test_consensus_apply_election_yes_nonowner() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();

        let election_request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards: vec![member(1), member(2), member(3)],
                    election_epoch: 5,
                },
            )),
        };

        // Non-owner: proposal was not stored in voting queue
        let proposal_id = 44;
        let payload = election_request.encode_to_vec();
        let result = apply_consensus_result(&mut group, proposal_id, true, &payload).unwrap();

        assert!(result.election.is_some());
        let outcome = result.election.unwrap();
        assert_eq!(outcome.election_epoch, 5);
        assert_eq!(outcome.proposed_stewards.len(), 3);

        // No proposals in queue
        assert_eq!(group.approved_proposals_count(), 0);
    }
}
