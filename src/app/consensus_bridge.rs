//! App-layer adapters over the consensus service: proposal submission, vote
//! casting, and forwarding peer messages in. Not protocol invariants — they
//! decide how consensus events reach the UI and the transport.

use alloy::signers::Signer;
use prost::Message;
use std::time::Duration;
use tracing::info;

use hashgraph_like_consensus::{
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    types::CreateProposalRequest,
};

use crate::app::error::UserError;
use crate::core::{CoreError, DeMlsProvider, Group, GroupEventHandler, ProviderConsensus};
use crate::protos::de_mls::messages::v1::{AppMessage, GroupUpdateRequest, VotePayload};

/// Submit a proposal to consensus. Returns `(proposal_id, vote_notification)`.
///
/// **Caller contract:** store ownership (`Group::store_voting_proposal`)
/// *before* emitting the notification via `on_app_message` — otherwise a
/// consensus result arriving immediately can see `is_owner=false`.
pub async fn submit_proposal<P: DeMlsProvider>(
    group_name: &str,
    request: &GroupUpdateRequest,
    expected_voters: u32,
    identity_string: String,
    consensus: &ProviderConsensus<P>,
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

/// Cast a vote and return the `AppMessage` to broadcast. Owners broadcast
/// the full `Proposal` (so peers see it for the first time); non-owners
/// broadcast just the `Vote`.
pub async fn cast_vote<P, SN>(
    group: &Group,
    proposal_id: u32,
    vote: bool,
    consensus: &ProviderConsensus<P>,
    signer: SN,
) -> Result<AppMessage, UserError>
where
    P: DeMlsProvider,
    SN: Signer + Send + Sync,
{
    let group_name = group.group_name();
    let is_owner = group.is_owner_of_proposal(proposal_id);
    let scope = P::Scope::from(group_name.to_string());

    let choice = if vote { "YES" } else { "NO" };
    let app_message: AppMessage = if is_owner {
        info!("[cast_vote]: Owner voting {choice} on proposal {proposal_id}");
        let proposal = consensus
            .cast_vote_and_get_proposal(&scope, proposal_id, vote, signer)
            .await?;
        proposal.into()
    } else {
        info!("[cast_vote]: User voting {choice} on proposal {proposal_id}");
        let vote_msg = consensus
            .cast_vote(&scope, proposal_id, vote, signer)
            .await?;
        vote_msg.into()
    };

    Ok(app_message)
}

/// Forward a peer's proposal into the local consensus service and emit a
/// `VotePayload` so the UI can surface the pending vote.
pub async fn forward_incoming_proposal<P: DeMlsProvider>(
    group_name: &str,
    proposal: Proposal,
    consensus: &ProviderConsensus<P>,
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

/// Forward a peer's vote into the local consensus service.
///
/// A late vote may arrive after our local session has been timeout-reaped
/// (`SessionNotActive`). That's a benign race — consensus concluded locally —
/// and we downgrade it to a debug log. Other consensus errors propagate.
pub async fn forward_incoming_vote<P: DeMlsProvider>(
    group_name: &str,
    vote: Vote,
    consensus: &ProviderConsensus<P>,
) -> Result<(), CoreError> {
    use hashgraph_like_consensus::error::ConsensusError;

    let scope = P::Scope::from(group_name.to_string());
    match consensus.process_incoming_vote(&scope, vote).await {
        Ok(()) => Ok(()),
        Err(ConsensusError::SessionNotActive) => {
            tracing::debug!(
                "[forward_incoming_vote] dropped late vote for group {group_name}: \
                 session already resolved"
            );
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
