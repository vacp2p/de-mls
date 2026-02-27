//! Consensus integration helpers for the app layer.
//!
//! These functions bridge the core MLS operations with the consensus voting
//! service and the `GroupEventHandler` event callbacks. They are app-layer
//! adapters — not protocol invariants — because they make concrete decisions
//! about how to wire consensus events to the UI (VotePayload emission) and
//! transport (outbound packet broadcasting).
//!
//! # Pure core vs app adapters
//!
//! [`apply_consensus_result`](crate::core::apply_consensus_result) is the only
//! truly pure consensus function and lives in `core`. Everything here requires
//! a running consensus service and a `GroupEventHandler` implementation.

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

use crate::app::error::UserError;
use crate::core::{CoreError, DeMlsProvider, GroupEventHandler, GroupHandle, build_message};
use crate::mls_crypto::{DeMlsStorage, MlsService};
use crate::protos::de_mls::messages::v1::{AppMessage, GroupUpdateRequest, VotePayload};

/// Create a consensus proposal for a group update request and start voting.
///
/// 1. Encodes the `GroupUpdateRequest` as the proposal payload
/// 2. Submits it to the consensus service
///
/// # Returns
/// `(proposal_id, vote_payload)` — the caller must:
/// 1. Store ownership via `GroupHandle::store_voting_proposal(proposal_id, request)`
/// 2. Then emit `vote_payload` via `GroupEventHandler::on_app_message`
///
/// The two-step caller contract eliminates a race where a consensus event for
/// `proposal_id` could arrive between submission and ownership storage.
pub async fn start_voting<P: DeMlsProvider>(
    group_name: &str,
    request: &GroupUpdateRequest,
    expected_voters: u32,
    identity_string: String,
    consensus: &P::Consensus,
) -> Result<(u32, AppMessage), CoreError> {
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

    Ok((proposal.proposal_id, vote_payload))
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
/// Handles two cases:
/// - **Proposal owner**: calls `cast_vote_and_get_proposal` and broadcasts the full `Proposal`
/// - **Non-owner**: calls `cast_vote` and broadcasts just the `Vote`
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
    app_id: &[u8],
) -> Result<(), UserError>
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

    let packet = build_message(handle, mls, &app_message, app_id)?;
    handler.on_outbound(group_name, packet).await?;
    Ok(())
}
