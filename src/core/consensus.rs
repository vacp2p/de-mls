//! Pure consensus result application.
//!
//! This module contains only [`apply_consensus_result`], which updates a
//! [`Group`]'s proposal state based on a consensus outcome.
//! It has no I/O, no service calls, and no event callbacks — it is a pure,
//! synchronous state transition.
//!
//! App-layer helpers that wire consensus events to the UI and transport
//! (`submit_proposal`, `cast_vote`, `forward_incoming_proposal`,
//! `forward_incoming_vote`) live in `crate::app::consensus_bridge`.

use prost::Message;
use tracing::info;

use crate::core::peer_scoring::{ScoreEvent, ScoreOp};
use crate::core::{CoreError, Group};
use crate::mls_crypto::ShortId;
use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, RemoveMember, StewardElectionProposal, ViolationEvidence, ViolationType,
    group_update_request,
};

/// Result of applying a consensus outcome: any score ops the app should feed
/// to its peer-scoring service, plus the accepted election proposal (if any)
/// for the app to validate and apply as a new steward list.
#[derive(Debug, Clone, Default)]
pub struct ConsensusApplyResult {
    pub score_ops: Vec<ScoreOp>,
    pub election: Option<StewardElectionProposal>,
}

/// Map a proto `ViolationType` to the corresponding target penalty `ScoreEvent`.
///
/// Callers gate `ScoreBelowThreshold` out upstream (the target is being removed
/// anyway), so only the three penalty kinds reach this function in practice.
/// Unknown / `Unspecified` wire values fall back to the harshest penalty.
fn target_score_event(violation_type: i32) -> ScoreEvent {
    match ViolationType::try_from(violation_type) {
        Ok(ViolationType::BrokenCommit) => ScoreEvent::BrokenCommit,
        Ok(ViolationType::BrokenMlsProposal) => ScoreEvent::BrokenMlsProposal,
        Ok(ViolationType::CensorshipInactivity) => ScoreEvent::CensorshipInactivity,
        _ => ScoreEvent::BrokenCommit,
    }
}

/// Accepted emergency → violation-specific target penalty + flat creator reward.
fn emergency_accepted_score_ops(evidence: &ViolationEvidence) -> [ScoreOp; 2] {
    [
        ScoreOp {
            member_id: evidence.target_member_id.clone(),
            event: target_score_event(evidence.violation_type),
        },
        ScoreOp {
            member_id: evidence.creator_member_id.clone(),
            event: ScoreEvent::EmergencyYesCreator,
        },
    ]
}

/// Rejected emergency → flat creator penalty (false accusation).
fn emergency_rejected_score_op(evidence: &ViolationEvidence) -> ScoreOp {
    ScoreOp {
        member_id: evidence.creator_member_id.clone(),
        event: ScoreEvent::EmergencyNoCreator,
    }
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

/// Build a `RemoveMember` `GroupUpdateRequest` for the target in score-below-threshold evidence.
fn removal_request_for(evidence: &ViolationEvidence) -> GroupUpdateRequest {
    GroupUpdateRequest {
        payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
            identity: evidence.target_member_id.clone(),
        })),
    }
}

/// Apply a consensus result to the group (pure, synchronous).
///
/// Determines ownership from the group and updates proposal state:
/// - **Approved + owner** → move to approved queue; if emergency, remove and score
/// - **Approved + non-owner** → insert into approved queue; if emergency, score only
/// - **Rejected + owner** → remove from voting queue; if emergency, score creator
/// - **Rejected + non-owner** → no group mutation; if emergency, score creator
///
/// `payload` is the serialized `GroupUpdateRequest` fetched from the consensus service.
/// Both owner and non-owner paths decode it to extract emergency evidence.
///
/// Returns a [`ConsensusApplyResult`] containing score ops (0, 1, or 2) that the
/// application layer should feed into a [`PeerScoringService`](crate::app::PeerScoringService).
pub fn apply_consensus_result(
    group: &mut Group,
    proposal_id: u32,
    approved: bool,
    payload: &[u8],
) -> Result<ConsensusApplyResult, CoreError> {
    let is_owner = group.is_owner_of_proposal(proposal_id);
    let request = GroupUpdateRequest::decode(payload)?;
    let evidence = extract_emergency_evidence(&request).cloned();
    let election = extract_election_proposal(&request).cloned();
    let is_emergency = evidence.is_some();
    let is_election = election.is_some();

    // ── Election proposals: no MLS operation, no score ops ──
    if is_election {
        if approved {
            if is_owner {
                // Move from voting to approved, then immediately remove (no MLS op).
                group.mark_proposal_as_approved(proposal_id);
                group.remove_approved_proposal(proposal_id);
            }
            // Non-owner: nothing to store — election proposals don't produce MLS ops.
            let election = election.unwrap();
            info!(
                proposal_id,
                epoch = election.election_epoch,
                stewards = election.proposed_stewards.len(),
                "steward election proposal accepted"
            );
            return Ok(ConsensusApplyResult {
                election: Some(election),
                ..Default::default()
            });
        } else {
            if is_owner {
                group.mark_proposal_as_rejected(proposal_id);
            }
            info!(proposal_id, "steward election proposal rejected");
            return Ok(ConsensusApplyResult::default());
        }
    }

    // ── Emergency and regular proposals ──

    // Should the approved ECP transform into a RemoveMember?
    let transforms_to_removal =
        approved && is_emergency && evidence.as_ref().is_some_and(is_score_below_threshold);

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
    } else if is_owner {
        group.mark_proposal_as_rejected(proposal_id);
    }

    match evidence {
        Some(ev) if approved => {
            info!(
                proposal_id,
                target = %ShortId(&ev.target_member_id),
                creator = %ShortId(&ev.creator_member_id),
                "emergency criteria proposal accepted"
            );
            let ops = if is_score_below_threshold(&ev) {
                // Target is already at/below threshold and will be removed —
                // skip the redundant penalty, only reward the creator.
                vec![ScoreOp {
                    member_id: ev.creator_member_id.clone(),
                    event: ScoreEvent::EmergencyYesCreator,
                }]
            } else {
                emergency_accepted_score_ops(&ev).into()
            };
            Ok(ConsensusApplyResult {
                score_ops: ops,
                ..Default::default()
            })
        }
        Some(ev) => {
            info!(
                proposal_id,
                creator = %ShortId(&ev.creator_member_id),
                "emergency criteria proposal rejected"
            );
            Ok(ConsensusApplyResult {
                score_ops: vec![emergency_rejected_score_op(&ev)],
                ..Default::default()
            })
        }
        None => Ok(ConsensusApplyResult::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::steward_list::{ProtocolConfig, StewardList};
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

    /// YES on an election the local node owns returns the accepted proposal,
    /// doesn't leave the proposal in the approved queue, and emits no score ops.
    #[test]
    fn election_yes_owner_returns_outcome_and_clears_queue() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config.clone()).unwrap();
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
        assert!(result.score_ops.is_empty());
        assert_eq!(group.approved_proposals_count(), 0);
    }

    /// NO on an election returns no outcome and leaves the approved queue empty.
    #[test]
    fn election_no_returns_empty() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        let request = election_request(vec![member(1), member(2)], 10);

        let proposal_id = 43;
        group.store_voting_proposal(proposal_id, request.clone());

        let result =
            apply_consensus_result(&mut group, proposal_id, false, &request.encode_to_vec())
                .unwrap();

        assert!(result.election.is_none());
        assert!(result.score_ops.is_empty());
        assert_eq!(group.approved_proposals_count(), 0);
    }

    /// YES on an election the local node *doesn't* own still returns the outcome
    /// (non-owner path), and doesn't touch any proposal queues.
    #[test]
    fn election_yes_nonowner_returns_outcome_without_queue_side_effects() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
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
}
