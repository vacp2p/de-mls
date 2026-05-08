//! Pure consensus result application.
//!
//! Updates a [`Group`]'s proposal queues in response to a consensus
//! outcome. No I/O, no service calls. Score deltas for emergency outcomes
//! are derived alongside in [`crate::core::emergency_score_ops`].
//!
//! App-layer helpers that wire consensus events to the UI and transport
//! (`submit_proposal`, `cast_vote`, `forward_incoming_proposal`,
//! `forward_incoming_vote`) live in `crate::app::consensus_bridge`.

use prost::Message;
use tracing::info;

use crate::{
    core::{CoreError, Group},
    identity::ShortId,
    protos::de_mls::messages::v1::{
        GroupUpdateRequest, RemoveMember, StewardElectionProposal, ViolationEvidence,
        ViolationType, group_update_request,
    },
};

/// Result of applying a consensus outcome. `force_freezing` signals
/// the app to skip the inactivity timer; `queued_remove_target` lets
/// the app refresh the steward list when the target is on it. Score
/// ops for emergency outcomes are derived by
/// [`emergency_score_ops`](crate::core::emergency_score_ops).
#[derive(Debug, Clone, Default)]
pub struct ConsensusApplyResult {
    pub election: Option<StewardElectionProposal>,
    pub force_freezing: bool,
    pub queued_remove_target: Option<Vec<u8>>,
    /// `true` when an accepted Layer-3 Deadlock ECP signals "open recovery
    /// mode." Caller flips the recovery-mode flag on; cleared on the next
    /// accepted election.
    pub enter_recovery_mode: bool,
}

/// Extract emergency evidence from a `GroupUpdateRequest`, if present.
fn extract_emergency_evidence(req: &GroupUpdateRequest) -> Option<&ViolationEvidence> {
    match &req.payload {
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => ec.evidence.as_ref(),
        _ => None,
    }
}

/// Extract a steward election proposal from a `GroupUpdateRequest`, if present.
fn extract_election_proposal(req: &GroupUpdateRequest) -> Option<&StewardElectionProposal> {
    match &req.payload {
        Some(group_update_request::Payload::StewardElection(se)) => Some(se),
        _ => None,
    }
}

/// Check whether evidence is a `SCORE_BELOW_THRESHOLD` violation.
fn is_score_below_threshold(evidence: &ViolationEvidence) -> bool {
    ViolationType::try_from(evidence.violation_type) == Ok(ViolationType::ScoreBelowThreshold)
}

/// Check whether evidence is the `DEADLOCK` (Layer 3 anti-deadlock) signal.
fn is_deadlock(evidence: &ViolationEvidence) -> bool {
    ViolationType::try_from(evidence.violation_type) == Ok(ViolationType::Deadlock)
}

/// Build a `RemoveMember` `GroupUpdateRequest` for the target in score-below-threshold evidence.
fn removal_request_for(evidence: &ViolationEvidence) -> GroupUpdateRequest {
    GroupUpdateRequest {
        payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
            identity: evidence.target_member_id.clone(),
        })),
    }
}

/// Identity this approval would queue for removal in `approved_proposals`,
/// if any. Covers a direct `RemoveMember` request and a score-below-threshold
/// ECP that transforms into one. Returns `None` for elections, non-removal
/// emergencies, non-removal regular proposals, and rejections.
fn pending_removal_target(
    request: &GroupUpdateRequest,
    evidence: Option<&ViolationEvidence>,
    approved: bool,
    is_emergency: bool,
    transforms_to_removal: bool,
) -> Option<Vec<u8>> {
    if !approved {
        return None;
    }
    if transforms_to_removal {
        return evidence.map(|ev| ev.target_member_id.clone());
    }
    if is_emergency {
        return None;
    }
    match request.payload.as_ref() {
        Some(group_update_request::Payload::RemoveMember(r)) => Some(r.identity.clone()),
        _ => None,
    }
}

/// Election outcome — no MLS operation. YES hands the proposed list
/// back to the app for validation and install; NO drops the owner's
/// voting-queue entry.
fn apply_election_outcome(
    group: &mut Group,
    proposal_id: u32,
    approved: bool,
    election: StewardElectionProposal,
    is_owner: bool,
) -> ConsensusApplyResult {
    if approved {
        if is_owner {
            group.mark_proposal_as_approved(proposal_id);
            group.remove_approved_proposal(proposal_id);
        }
        info!(
            proposal_id,
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            "steward election proposal accepted"
        );
        ConsensusApplyResult {
            election: Some(election),
            ..Default::default()
        }
    } else {
        if is_owner {
            group.mark_proposal_as_rejected(proposal_id);
        }
        info!(proposal_id, "steward election proposal rejected");
        ConsensusApplyResult::default()
    }
}

/// Apply a consensus result to the group's proposal queues.
///
/// Routes by proposal kind:
/// - **Election (accepted)** — returned via `election` for the app to
///   validate and install.
/// - **`ScoreBelowThreshold` ECP (accepted)** — queues `RemoveMember(target)`
///   in the approved queue, sets the urgent-commit target, and signals
///   `force_freezing` so the urgent commit fires now.
/// - **`Deadlock` ECP (accepted)** — opens `recovery_mode` (any-member
///   commit) and signals `force_freezing`.
/// - **Other emergency (accepted)** — transient in the approved queue:
///   briefly marked approved then removed. No MLS op to commit.
/// - **Regular proposal (accepted)** — moved to the approved queue.
///   `RemoveMember` for an already-queued target is deduped at insertion.
/// - **Rejected (any kind)** — dropped from the voting queue if we
///   owned it.
pub fn apply_consensus_result(
    group: &mut Group,
    proposal_id: u32,
    approved: bool,
    payload: &[u8],
) -> Result<ConsensusApplyResult, CoreError> {
    let is_owner = group.is_owner_of_proposal(proposal_id);
    let request = GroupUpdateRequest::decode(payload)?;
    let evidence = extract_emergency_evidence(&request).cloned();
    let is_emergency = evidence.is_some();

    if let Some(election) = extract_election_proposal(&request).cloned() {
        return Ok(apply_election_outcome(
            group,
            proposal_id,
            approved,
            election,
            is_owner,
        ));
    }

    // ── Emergency and regular proposals ──

    // Should the approved ECP transform into a RemoveMember?
    let transforms_to_removal =
        approved && is_emergency && evidence.as_ref().is_some_and(is_score_below_threshold);

    // Used for target-keyed dedup below and reported back so the app
    // layer can fire a steward-list refresh.
    let removal_target = pending_removal_target(
        &request,
        evidence.as_ref(),
        approved,
        is_emergency,
        transforms_to_removal,
    );

    // Two approvals from independent paths (self-leave + ban, ECP + ban, …)
    // can each carry `RemoveMember(target)` under different proposal ids.
    // MLS rejects a duplicate removal at commit time, so keep the first
    // entry and drop the second.
    if let Some(target) = &removal_target
        && group.is_pending_removal(target)
    {
        if is_owner {
            group.mark_proposal_as_rejected(proposal_id);
        }
        info!(
            proposal_id,
            target = %ShortId(target),
            "removal proposal deduped — target already queued for removal"
        );
        return Ok(ConsensusApplyResult::default());
    }

    let mut force_freezing = false;
    let mut enter_recovery_mode = false;

    if approved {
        if is_owner {
            group.mark_proposal_as_approved(proposal_id);
            if transforms_to_removal {
                // Replace ECP with RemoveMember in approved queue (reuse proposal_id).
                let removal = removal_request_for(evidence.as_ref().unwrap());
                group.remove_approved_proposal(proposal_id);
                group.insert_approved_proposal(proposal_id, removal);
            } else if is_emergency {
                // Other emergencies don't produce MLS operations.
                group.remove_approved_proposal(proposal_id);
            }
        } else if transforms_to_removal {
            // Non-owner: insert RemoveMember directly (the ECP was never stored).
            let removal = removal_request_for(evidence.as_ref().unwrap());
            group.insert_approved_proposal(proposal_id, removal);
        } else if !is_emergency {
            // Regular proposal: add to approved queue for the next commit.
            group.insert_approved_proposal(proposal_id, request);
        }

        if transforms_to_removal {
            // Fast removal: restrict the next commit to this target so it
            // doesn't drag along unrelated approved work.
            let target = evidence.as_ref().unwrap().target_member_id.clone();
            group.set_urgent_commit_target(target);
            force_freezing = true;
        } else if evidence.as_ref().is_some_and(is_deadlock) {
            // Layer 3: relax the steward gate so any member can produce
            // the next commit. App caller flips `GroupEntry::recovery_mode`
            // when it sees this flag; cleared when a fresh election lands.
            enter_recovery_mode = true;
            force_freezing = true;
        }
    } else if is_owner {
        group.mark_proposal_as_rejected(proposal_id);
    }

    if let Some(ev) = evidence.as_ref() {
        if approved {
            info!(
                proposal_id,
                target = %ShortId(&ev.target_member_id),
                creator = %ShortId(&ev.creator_member_id),
                "emergency criteria proposal accepted"
            );
        } else {
            info!(
                proposal_id,
                creator = %ShortId(&ev.creator_member_id),
                "emergency criteria proposal rejected"
            );
        }
    }

    Ok(ConsensusApplyResult {
        election: None,
        force_freezing,
        queued_remove_target: removal_target,
        enter_recovery_mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::steward_list_plugin::{StewardList, StewardListConfig};
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

    fn make_group(name: &str, identity: Vec<u8>) -> Group {
        Group::create_group(name, identity)
    }

    fn election_request(stewards: Vec<Vec<u8>>, epoch: u64) -> GroupUpdateRequest {
        GroupUpdateRequest {
            payload: Some(group_update_request::Payload::StewardElection(
                StewardElectionProposal {
                    proposed_stewards: stewards,
                    election_epoch: epoch,
                    retry_round: 0,
                },
            )),
        }
    }

    /// YES on an election the local node owns returns the accepted proposal
    /// and doesn't leave the proposal in the approved queue.
    #[test]
    fn election_yes_owner_returns_outcome_and_clears_queue() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mut group = make_group("test-group", member(1));
        let mems = members(&[1, 2, 3, 4, 5]);
        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-group", &mems, sn, config, 0).unwrap();
        let request = election_request(list.members().to_vec(), 10);

        let proposal_id = 42;
        group.store_voting_proposal(proposal_id, request.clone());

        let result =
            apply_consensus_result(&mut group, proposal_id, true, &request.encode_to_vec())
                .unwrap();

        let outcome = result.election.expect("election outcome expected");
        assert_eq!(outcome.election_epoch, 10);
        assert_eq!(outcome.proposed_stewards.len(), 5);
        assert_eq!(group.approved_proposals_count(), 0);
    }

    /// NO on an election returns no outcome and leaves the approved queue empty.
    #[test]
    fn election_no_returns_empty() {
        let mut group = make_group("test-group", member(1));
        let request = election_request(vec![member(1), member(2)], 10);

        let proposal_id = 43;
        group.store_voting_proposal(proposal_id, request.clone());

        let result =
            apply_consensus_result(&mut group, proposal_id, false, &request.encode_to_vec())
                .unwrap();

        assert!(result.election.is_none());
        assert_eq!(group.approved_proposals_count(), 0);
    }

    /// YES on an election the local node *doesn't* own still returns the outcome
    /// (non-owner path), and doesn't touch any proposal queues.
    #[test]
    fn election_yes_nonowner_returns_outcome_without_queue_side_effects() {
        let mut group = make_group("test-group", member(1));
        let request = election_request(vec![member(1), member(2), member(3)], 5);

        let proposal_id = 44;
        let result =
            apply_consensus_result(&mut group, proposal_id, true, &request.encode_to_vec())
                .unwrap();

        let outcome = result.election.expect("election outcome expected");
        assert_eq!(outcome.election_epoch, 5);
        assert_eq!(outcome.proposed_stewards.len(), 3);
        assert_eq!(group.approved_proposals_count(), 0);
    }

    fn remove_request(target: Vec<u8>) -> GroupUpdateRequest {
        GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: target,
            })),
        }
    }

    /// A second `RemoveMember(target)` arriving via consensus is dropped
    /// when an entry for the same target is already in `approved_proposals`.
    #[test]
    fn removal_deduped_when_target_already_pending() {
        let mut group = make_group("test-group", member(1));
        let target = member(7);

        // First removal — non-owner path inserts straight into approved.
        let first_id = 10;
        let request = remove_request(target.clone());
        apply_consensus_result(&mut group, first_id, true, &request.encode_to_vec()).unwrap();
        assert_eq!(group.approved_proposals_count(), 1);

        // Second removal for the same target arrives under a different id.
        let second_id = 11;
        let request = remove_request(target.clone());
        let result =
            apply_consensus_result(&mut group, second_id, true, &request.encode_to_vec()).unwrap();

        assert!(result.election.is_none());
        assert_eq!(
            group.approved_proposals_count(),
            1,
            "duplicate removal must not stack a second entry"
        );
        assert!(group.approved_proposals().contains_key(&first_id));
        assert!(!group.approved_proposals().contains_key(&second_id));
    }

    /// Owner-side dedup: the duplicate clears its voting-queue entry so the
    /// queue does not retain an outcome we deliberately discarded.
    #[test]
    fn removal_dedup_clears_owner_voting_entry() {
        let mut group = make_group("test-group", member(1));
        let target = member(7);

        // Pre-existing approved removal from an unrelated path.
        let pending_id = 20;
        let pending = remove_request(target.clone());
        group.insert_approved_proposal(pending_id, pending);

        // This user submits their own removal and it passes consensus.
        let owner_id = 21;
        let owner_request = remove_request(target.clone());
        group.store_voting_proposal(owner_id, owner_request.clone());

        let result =
            apply_consensus_result(&mut group, owner_id, true, &owner_request.encode_to_vec())
                .unwrap();

        assert!(result.election.is_none());
        assert_eq!(group.approved_proposals_count(), 1);
        assert!(group.approved_proposals().contains_key(&pending_id));
        assert!(
            !group.is_owner_of_proposal(owner_id),
            "duplicate must be cleared from voting queue"
        );
    }

    fn score_below_threshold_request(target: Vec<u8>, creator: Vec<u8>) -> GroupUpdateRequest {
        ViolationEvidence::score_below_threshold(target, 0, -10)
            .with_creator(creator)
            .into_update_request()
            .unwrap()
    }

    #[test]
    fn ecp_score_below_threshold_yes_marks_urgent_and_force_freezes() {
        let mut group = make_group("urgent-yes", member(1));
        let target = member(7);

        let request = score_below_threshold_request(target.clone(), member(1));
        let payload = request.encode_to_vec();

        let result = apply_consensus_result(&mut group, 100, true, &payload).unwrap();

        assert!(result.force_freezing, "ECP YES must signal force-Freezing");
        assert!(result.election.is_none());
        assert_eq!(
            group.urgent_commit_target(),
            Some(target.as_slice()),
            "urgent-commit target must be set on the group"
        );
        assert_eq!(group.approved_proposals_count(), 1, "RemoveMember queued");
    }

    #[test]
    fn ecp_score_below_threshold_no_does_not_mark_urgent() {
        let mut group = make_group("urgent-no", member(1));
        let request = score_below_threshold_request(member(7), member(1));
        let payload = request.encode_to_vec();

        let result = apply_consensus_result(&mut group, 101, false, &payload).unwrap();

        assert!(!result.force_freezing);
        assert!(group.urgent_commit_target().is_none());
        assert_eq!(group.approved_proposals_count(), 0);
    }

    fn deadlock_request(creator: Vec<u8>) -> GroupUpdateRequest {
        ViolationEvidence::deadlock(0)
            .with_creator(creator)
            .into_update_request()
            .unwrap()
    }

    #[test]
    fn ecp_deadlock_yes_signals_recovery_mode_and_force_freezes() {
        let mut group = make_group("deadlock-yes", member(1));

        let request = deadlock_request(member(1));
        let payload = request.encode_to_vec();
        let result = apply_consensus_result(&mut group, 200, true, &payload).unwrap();

        assert!(
            result.force_freezing,
            "Deadlock YES must signal force-Freezing"
        );
        assert!(
            result.enter_recovery_mode,
            "Deadlock YES must signal recovery-mode open"
        );
        assert_eq!(
            group.approved_proposals_count(),
            0,
            "Deadlock has no specific target — no RemoveMember queued"
        );
        assert!(group.urgent_commit_target().is_none());
    }

    #[test]
    fn ecp_deadlock_no_does_not_signal_recovery_mode() {
        let mut group = make_group("deadlock-no", member(1));

        let request = deadlock_request(member(1));
        let payload = request.encode_to_vec();
        let result = apply_consensus_result(&mut group, 201, false, &payload).unwrap();

        assert!(!result.force_freezing);
        assert!(!result.enter_recovery_mode);
    }
}
