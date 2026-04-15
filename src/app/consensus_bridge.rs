//! Consensus integration helpers for the app layer.
//!
//! These functions bridge the core MLS operations with the consensus voting
//! service and the `GroupEventHandler` event callbacks. They are app-layer
//! adapters — not protocol invariants — because they make concrete decisions
//! about how to wire consensus events to the UI (VotePayload emission) and
//! transport (outbound packet broadcasting).
//!
use alloy::signers::Signer;
use prost::Message;
use std::time::Duration;
use tracing::info;

use hashgraph_like_consensus::{
    api::ConsensusServiceAPI,
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    types::CreateProposalRequest,
};

use crate::app::error::UserError;
use crate::core::{CoreError, DeMlsProvider, Group, GroupEventHandler};
use crate::protos::de_mls::messages::v1::{AppMessage, GroupUpdateRequest, VotePayload};

/// Create a consensus proposal for a group update request and start voting.
///
/// 1. Encodes the `GroupUpdateRequest` as the proposal payload
/// 2. Submits it to the consensus service
///
/// # Returns
/// `(proposal_id, vote_notification)` — the caller must:
/// 1. Store ownership via `Group::store_voting_proposal(proposal_id, request)`
/// 2. Then emit `vote_notification` via `GroupEventHandler::on_app_message`
///
/// The two-step caller contract eliminates a race where a consensus event for
/// `proposal_id` could arrive between submission and ownership storage.
pub async fn submit_proposal<P: DeMlsProvider>(
    group_name: &str,
    request: &GroupUpdateRequest,
    expected_voters: u32,
    identity_string: String,
    consensus: &P::Consensus,
    proposal_expiration: Duration,
    consensus_timeout: Duration,
) -> Result<(u32, AppMessage), CoreError> {
    let payload = request.encode_to_vec();
    let create_request = CreateProposalRequest::new(
        uuid::Uuid::new_v4().to_string(),
        payload.clone(),
        identity_string.into(),
        expected_voters,
        proposal_expiration.as_secs(),
        true,
    )?;

    let scope = P::Scope::from(group_name.to_string());
    let proposal = consensus
        .create_proposal_with_config(
            &scope,
            create_request,
            Some(ConsensusConfig::gossipsub().with_timeout(consensus_timeout)?),
        )
        .await?;

    info!(
        "[submit_proposal]: Created proposal {} with {} expected voters",
        proposal.proposal_id, expected_voters
    );

    let vote_notification: AppMessage = VotePayload {
        group_id: group_name.to_string(),
        proposal_id: proposal.proposal_id,
        payload,
        timestamp: proposal.timestamp,
    }
    .into();

    Ok((proposal.proposal_id, vote_notification))
}

/// Cast a vote on a proposal and return it to broadcast it to the group.
///
/// Handles two cases:
/// - **Proposal owner**: calls `cast_vote_and_get_proposal` and return the full `Proposal`
/// - **Non-owner**: calls `cast_vote` and return just the `Vote`
pub async fn cast_vote<P, SN>(
    group: &Group,
    proposal_id: u32,
    vote: bool,
    consensus: &P::Consensus,
    signer: SN,
) -> Result<AppMessage, UserError>
where
    P: DeMlsProvider,
    SN: Signer + Send + Sync,
{
    let group_name = group.group_name();
    let is_owner = group.is_owner_of_proposal(proposal_id);
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

    Ok(app_message)
}

/// Forward a proposal received from another peer to the consensus service.
///
/// When a peer broadcasts their proposal, other members receive it as an MLS
/// application message. This function forwards it to the local consensus service
/// and emits a `VotePayload` so the UI can display the pending vote.
pub async fn forward_incoming_proposal<P: DeMlsProvider>(
    group_name: &str,
    proposal: Proposal,
    consensus: &P::Consensus,
    handler: &dyn GroupEventHandler,
) -> Result<(), UserError> {
    let scope = P::Scope::from(group_name.to_string());
    consensus
        .process_incoming_proposal(&scope, proposal.clone())
        .await?;

    let vote_notification: AppMessage = VotePayload {
        group_id: group_name.to_string(),
        proposal_id: proposal.proposal_id,
        payload: proposal.payload.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    }
    .into();

    handler
        .on_app_message(group_name, vote_notification)
        .await?;
    Ok(())
}

/// Forward a vote received from another peer to the consensus service.
pub async fn forward_incoming_vote<P: DeMlsProvider>(
    group_name: &str,
    vote: Vote,
    consensus: &P::Consensus,
) -> Result<(), CoreError> {
    let scope = P::Scope::from(group_name.to_string());
    consensus.process_incoming_vote(&scope, vote).await?;
    Ok(())
}
