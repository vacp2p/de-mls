//! Consensus integration for DE-MLS group operations.
//!
//! This module bridges MLS group operations with the consensus voting layer.
//! It provides functions for creating proposals, casting votes, and applying
//! consensus outcomes.
//!
//! # Overview
//!
//! When a membership change is requested (join/remove), it goes through consensus:
//!
//! ```text
//! 1. Steward receives key package → ProcessResult::GetUpdateRequest
//! 2. App matches ProcessResult directly and starts voting
//! 3. Users vote via cast_vote() → votes sent as MLS messages
//! 4. Consensus service emits outcome
//! 5. App calls apply_consensus_result() → updates proposal state
//! 6. Steward calls create_commit_candidate() → broadcasts commit candidate
//! ```
//!
//! # Key Functions
//!
//! ## Voting Workflow
//! - [`start_voting`] - Create a proposal and start voting
//! - [`cast_vote`] - Cast a vote on a proposal
//! - [`apply_consensus_result`] - Apply a consensus outcome to the handle (pure, sync)
//!
//! ## Message Forwarding (optional integration helpers)
//! - [`forward_incoming_proposal`] - Forward received proposals to consensus
//! - [`forward_incoming_vote`] - Forward received votes to consensus

use alloy::signers::Signer;
use openmls_rust_crypto::MemoryStorage;
use prost::Message;
use std::time::Duration;
use tracing::info;

use hashgraph_like_consensus::{
    api::ConsensusServiceAPI,
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    types::CreateProposalRequest,
};

use crate::core::{CoreError, DeMlsProvider, GroupEventHandler, GroupHandle, build_message};
use crate::mls_crypto::{DeMlsStorage, MlsService};
use crate::protos::de_mls::messages::v1::{
    AppMessage, GroupUpdateRequest, VotePayload, group_update_request,
};

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

/// Create a consensus proposal for a group update request and start voting.
///
/// This is an optional integration helper. Core-only integrators can use their own
/// consensus implementation and call `apply_consensus_result` directly.
///
/// This function:
/// 1. Creates a `CreateProposalRequest` with the encoded group update
/// 2. Submits it to the consensus service
/// 3. Emits a `VotePayload` via the handler for UI display
///
/// # Returns
/// The proposal ID, which should be stored via `GroupHandle::store_voting_proposal`.
pub async fn start_voting<P: DeMlsProvider>(
    group_name: &str,
    request: &GroupUpdateRequest,
    expected_voters: u32,
    identity_string: String,
    consensus: &P::Consensus,
    handler: &dyn GroupEventHandler,
) -> Result<u32, CoreError> {
    let payload = request.encode_to_vec();
    let create_request = CreateProposalRequest::new(
        uuid::Uuid::new_v4().to_string(),
        payload.clone(),
        identity_string.into(),
        expected_voters,
        3600,
        true,
    )?;

    let scope = P::Scope::from(group_name.to_string());
    let proposal = consensus
        .create_proposal_with_config(
            &scope,
            create_request,
            Some(ConsensusConfig::gossipsub().with_timeout(Duration::from_secs(15))?),
        )
        .await?;

    info!(
        "[start_voting]: Created proposal {} with {} expected voters",
        proposal.proposal_id, expected_voters
    );

    let vote_payload: AppMessage = VotePayload {
        group_id: group_name.to_string(),
        proposal_id: proposal.proposal_id,
        payload,
        timestamp: proposal.timestamp,
    }
    .into();

    handler.on_app_message(group_name, vote_payload).await?;

    Ok(proposal.proposal_id)
}

/// Forward a proposal received from another peer to the consensus service.
///
/// Optional integration helper. When a peer broadcasts their proposal, other members
/// receive it as an MLS application message. This function forwards it to the local
/// consensus service and emits a `VotePayload` so the UI can display the pending vote.
pub async fn forward_incoming_proposal<P: DeMlsProvider>(
    group_name: &str,
    proposal: Proposal,
    consensus: &P::Consensus,
    handler: &dyn GroupEventHandler,
) -> Result<(), CoreError> {
    let scope = P::Scope::from(group_name.to_string());
    consensus
        .process_incoming_proposal(&scope, proposal.clone())
        .await?;

    let vote_payload: AppMessage = VotePayload {
        group_id: group_name.to_string(),
        proposal_id: proposal.proposal_id,
        payload: proposal.payload.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    }
    .into();

    handler.on_app_message(group_name, vote_payload).await?;
    Ok(())
}

/// Forward a vote received from another peer to the consensus service.
///
/// Optional integration helper.
pub async fn forward_incoming_vote<P: DeMlsProvider>(
    group_name: &str,
    vote: Vote,
    consensus: &P::Consensus,
) -> Result<(), CoreError> {
    let scope = P::Scope::from(group_name.to_string());
    consensus.process_incoming_vote(&scope, vote).await?;
    Ok(())
}

/// Cast a vote on a proposal and broadcast it to the group.
///
/// Optional integration helper. Handles two cases:
/// - **Proposal owner**: Calls `cast_vote_and_get_proposal` and broadcasts the full `Proposal`
/// - **Non-owner**: Calls `cast_vote` and broadcasts just the `Vote`
#[allow(clippy::too_many_arguments)]
pub async fn cast_vote<P, SN, S>(
    handle: &GroupHandle,
    group_name: &str,
    proposal_id: u32,
    vote: bool,
    consensus: &P::Consensus,
    signer: SN,
    mls: &MlsService<S>,
    handler: &dyn GroupEventHandler,
) -> Result<(), CoreError>
where
    P: DeMlsProvider,
    SN: Signer + Send + Sync,
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let is_owner = handle.is_owner_of_proposal(proposal_id);
    let scope = P::Scope::from(group_name.to_string());

    let app_message: AppMessage = if is_owner {
        info!("[cast_vote]: Owner voting on proposal {proposal_id}");
        let proposal = consensus
            .cast_vote_and_get_proposal(&scope, proposal_id, vote, signer)
            .await?;
        proposal.into()
    } else {
        info!("[cast_vote]: User voting on proposal {proposal_id}");
        let vote_msg = consensus
            .cast_vote(&scope, proposal_id, vote, signer)
            .await?;
        vote_msg.into()
    };

    let packet = build_message(handle, mls, &app_message)?;
    handler.on_outbound(group_name, packet).await?;
    Ok(())
}
