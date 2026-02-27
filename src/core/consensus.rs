//! Pure consensus result application.
//!
//! This module contains only [`apply_consensus_result`], which updates a
//! [`GroupHandle`]'s proposal state based on a typed [`ConsensusOutcome`].
//! It has no I/O, no service calls, and no event callbacks — it is a pure,
//! synchronous state transition.
//!
//! App-layer helpers that wire consensus events to the UI and transport
//! (`start_voting`, `cast_vote`, `forward_incoming_proposal`,
//! `forward_incoming_vote`) live in `crate::app::consensus`.

use prost::Message;
use tracing::info;

use crate::core::{CoreError, GroupHandle};
use crate::protos::de_mls::messages::v1::{GroupUpdateRequest, group_update_request};

/// Typed outcome of a consensus decision.
///
/// Prevents invalid state combinations at the type level:
/// - `Approved { payload }` requires the caller to have fetched the payload (non-owner)
/// - `ApprovedOwner` signals the payload is already in the handle (owner)
/// - `Rejected` applies to both owner and non-owner
#[derive(Debug)]
pub enum ConsensusOutcome<'a> {
    /// Consensus approved. Caller (non-owner) fetched the payload.
    Approved { payload: &'a [u8] },
    /// Consensus approved. The handle already has the payload (owner).
    ApprovedOwner,
    /// Consensus rejected (or failed).
    Rejected,
}

/// Apply a consensus result to the group handle (pure, synchronous).
///
/// Updates the handle's proposal state based on the outcome:
///
/// - `ApprovedOwner` → verify ownership, mark proposal as approved, handle emergency removal
/// - `Approved { payload }` → verify non-ownership, decode and insert approved proposal
/// - `Rejected` → mark proposal as rejected (if owned) or log (if not owned)
///
/// # Defense-in-depth
///
/// Even though `ConsensusOutcome` prevents most invalid combinations at the type level,
/// this function validates assumptions against handle state:
/// - `ApprovedOwner` → verifies `handle.is_owner_of_proposal(id)`
/// - `Approved { payload }` → verifies `!handle.is_owner_of_proposal(id)`
/// - Unknown `proposal_id` for owner paths → returns `CoreError::ProposalNotFound`
pub fn apply_consensus_result(
    handle: &mut GroupHandle,
    proposal_id: u32,
    outcome: ConsensusOutcome<'_>,
) -> Result<(), CoreError> {
    match outcome {
        ConsensusOutcome::ApprovedOwner => {
            if !handle.is_owner_of_proposal(proposal_id) {
                if handle.has_approved_proposal(proposal_id) {
                    return Err(CoreError::InvalidConsensusOutcome(format!(
                        "ApprovedOwner for proposal {proposal_id} but proposal is already in approved queue"
                    )));
                }
                return Err(CoreError::ProposalNotFound(proposal_id));
            }

            handle.mark_proposal_as_approved(proposal_id);

            // Emergency proposals don't produce MLS operations — remove from approved queue.
            let approved = handle.approved_proposals();
            if let Some(req) = approved.get(&proposal_id) {
                if matches!(
                    req.payload,
                    Some(group_update_request::Payload::EmergencyCriteria(_))
                ) {
                    info!(
                        "Emergency criteria proposal {proposal_id} ACCEPTED (owner). \
                         TODO (Milestone 2): apply peer score penalty to target."
                    );
                    handle.remove_approved_proposal(proposal_id);
                }
            }
        }
        ConsensusOutcome::Approved { payload } => {
            if handle.is_owner_of_proposal(proposal_id) {
                return Err(CoreError::InvalidConsensusOutcome(format!(
                    "Approved {{ payload }} for proposal {proposal_id} but handle owns it — use ApprovedOwner"
                )));
            }

            let update_request = GroupUpdateRequest::decode(payload)?;

            if let Some(group_update_request::Payload::EmergencyCriteria(ref ec)) =
                update_request.payload
            {
                info!(
                    "Emergency criteria proposal {proposal_id} ACCEPTED (non-owner): \
                     violation_type={:?}. TODO (Milestone 2): apply peer score penalty to target.",
                    ec.evidence.as_ref().map(|e| e.violation_type)
                );
                // Emergency proposals don't produce MLS operations — don't add to approved.
            } else {
                handle.insert_approved_proposal(proposal_id, update_request);
            }
        }
        ConsensusOutcome::Rejected => {
            if handle.is_owner_of_proposal(proposal_id) {
                handle.mark_proposal_as_rejected(proposal_id);
            } else {
                // !result && !is_owner: proposal rejected
                // TODO (Milestone 2): If emergency criteria, apply false accusation penalty to creator.
                info!("Proposal {proposal_id} rejected (not owner, no local state to update)");
            }
        }
    }

    Ok(())
}
