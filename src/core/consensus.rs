//! Pure consensus result application.
//!
//! This module contains only [`apply_consensus_result`], which updates a
//! [`GroupHandle`]'s proposal state based on a consensus outcome.
//! It has no I/O, no service calls, and no event callbacks — it is a pure,
//! synchronous state transition.
//!
//! App-layer helpers that wire consensus events to the UI and transport
//! (`start_voting`, `cast_vote`, `forward_incoming_proposal`,
//! `forward_incoming_vote`) live in `crate::app::consensus`.

use prost::Message;
use tracing::info;

use crate::core::peer_scoring::{ConsensusApplyResult, ScoreEvent, ScoreOp};
use crate::core::{CoreError, GroupHandle};
use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, RemoveMember, ViolationEvidence, ViolationType, group_update_request,
};

/// Map a proto `ViolationType` to the corresponding target penalty `ScoreEvent`.
fn target_score_event(violation_type: i32) -> ScoreEvent {
    match ViolationType::try_from(violation_type) {
        Ok(ViolationType::BrokenCommit) => ScoreEvent::BrokenCommit,
        Ok(ViolationType::BrokenMlsProposal) => ScoreEvent::BrokenMlsProposal,
        Ok(ViolationType::CensorshipInactivity) => ScoreEvent::CensorshipInactivity,
        Ok(ViolationType::ScoreBelowThreshold) => ScoreEvent::ScoreBelowThreshold,
        // Unspecified or unknown — fall back to the most severe penalty.
        _ => ScoreEvent::BrokenCommit,
    }
}

/// Score ops for an accepted emergency: penalize target (violation-specific), reward creator.
/// Always returns exactly 2 ops.
fn emergency_accepted_score_ops(evidence: &ViolationEvidence) -> [ScoreOp; 2] {
    [
        ScoreOp::new(
            evidence.target_member_id.clone(),
            target_score_event(evidence.violation_type),
        ),
        ScoreOp::new(
            evidence.creator_member_id.clone(),
            ScoreEvent::EmergencyYesCreator,
        ),
    ]
}

/// Score op for a rejected emergency: penalize creator (false accusation).
/// Always returns exactly 1 op.
fn emergency_rejected_score_op(evidence: &ViolationEvidence) -> ScoreOp {
    ScoreOp::new(
        evidence.creator_member_id.clone(),
        ScoreEvent::EmergencyNoCreator,
    )
}

/// Extract emergency evidence from a `GroupUpdateRequest`, if present.
fn extract_emergency_evidence(req: &GroupUpdateRequest) -> Option<&ViolationEvidence> {
    match &req.payload {
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => ec.evidence.as_ref(),
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

/// Apply a consensus result to the group handle (pure, synchronous).
///
/// Determines ownership from the handle and updates proposal state:
/// - **Approved + owner** → move to approved queue; if emergency, remove and score
/// - **Approved + non-owner** → insert into approved queue; if emergency, score only
/// - **Rejected + owner** → remove from voting queue; if emergency, score creator
/// - **Rejected + non-owner** → no handle mutation; if emergency, score creator
///
/// `payload` is the serialized `GroupUpdateRequest` fetched from the consensus service.
/// Both owner and non-owner paths decode it to extract emergency evidence.
///
/// Returns a [`ConsensusApplyResult`] containing score ops (0, 1, or 2) that the
/// application layer should feed into a [`PeerScoringService`](crate::app::PeerScoringService).
pub fn apply_consensus_result(
    handle: &mut GroupHandle,
    proposal_id: u32,
    approved: bool,
    payload: &[u8],
) -> Result<ConsensusApplyResult, CoreError> {
    let is_owner = handle.is_owner_of_proposal(proposal_id);
    let request = GroupUpdateRequest::decode(payload)?;
    let evidence = extract_emergency_evidence(&request).cloned();
    let is_emergency = evidence.is_some();

    // Should the approved ECP transform into a RemoveMember?
    let transforms_to_removal =
        approved && is_emergency && evidence.as_ref().is_some_and(is_score_below_threshold);

    // Mutate handle state.
    if approved {
        if is_owner {
            handle.mark_proposal_as_approved(proposal_id);
            if transforms_to_removal {
                // Replace ECP with RemoveMember in approved queue (reuse proposal_id).
                let removal = removal_request_for(evidence.as_ref().unwrap());
                handle.remove_approved_proposal(proposal_id);
                handle.insert_approved_proposal(proposal_id, removal);
            } else if is_emergency {
                // Other emergencies don't produce MLS operations.
                handle.remove_approved_proposal(proposal_id);
            }
        } else if transforms_to_removal {
            // Non-owner: insert RemoveMember directly (the ECP was never stored).
            let removal = removal_request_for(evidence.as_ref().unwrap());
            handle.insert_approved_proposal(proposal_id, removal);
        } else if !is_emergency {
            // Regular proposal: add to approved queue for the next commit.
            handle.insert_approved_proposal(proposal_id, request);
        }
    } else if is_owner {
        handle.mark_proposal_as_rejected(proposal_id);
    }

    // Produce score ops from evidence.
    match evidence {
        Some(ev) if approved => {
            info!(
                "Emergency criteria proposal {proposal_id} ACCEPTED: \
                 target={:?}, creator={:?}",
                ev.target_member_id, ev.creator_member_id
            );
            Ok(ConsensusApplyResult::with_ops(
                emergency_accepted_score_ops(&ev).into(),
            ))
        }
        Some(ev) => {
            info!(
                "Emergency criteria proposal {proposal_id} REJECTED: \
                 creator={:?}",
                ev.creator_member_id
            );
            Ok(ConsensusApplyResult::with_ops(vec![
                emergency_rejected_score_op(&ev),
            ]))
        }
        None => Ok(ConsensusApplyResult::empty()),
    }
}
