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

/// Consensus-session parameters that come from `GroupConfig`. Grouped so
/// [`submit_proposal`]'s argument list stays readable.
pub struct ProposalParams {
    pub expected_voters: u32,
    pub proposal_expiration: Duration,
    pub consensus_timeout: Duration,
    pub liveness_criteria_yes: bool,
}

/// Open a consensus session for `request`; return its `proposal_id` and the
/// unbundled `Proposal` wire message.
///
/// No vote is cast here. The caller decides whether to:
/// - bundle their vote by calling [`cast_vote`] (owner path returns the
///   proposal with the creator's vote attached), or
/// - broadcast the unbundled message as-is and let the creator vote later
///   like any other member.
///
/// In both cases the caller must record ownership in the group *before*
/// casting, so a single-voter consensus transition can't fire before
/// `Group::is_owner_of_proposal` is true.
pub async fn submit_proposal<P: DeMlsProvider>(
    group_name: &str,
    request: &GroupUpdateRequest,
    identity_string: String,
    consensus: &ProviderConsensus<P>,
    params: ProposalParams,
) -> Result<(u32, AppMessage), CoreError> {
    let payload = request.encode_to_vec();
    let create_request = CreateProposalRequest::new(
        uuid::Uuid::new_v4().to_string(),
        payload,
        identity_string.into(),
        params.expected_voters,
        params.proposal_expiration.as_secs(),
        params.liveness_criteria_yes,
    )?;

    let scope = P::Scope::from(group_name.to_string());
    let proposal = consensus
        .create_proposal_with_config(
            &scope,
            create_request,
            Some(ConsensusConfig::gossipsub().with_timeout(params.consensus_timeout)?),
        )
        .await?;

    info!(
        group = group_name,
        proposal_id = proposal.proposal_id,
        voters = params.expected_voters,
        "proposal opened"
    );

    let proposal_id = proposal.proposal_id;
    Ok((proposal_id, proposal.into()))
}

/// Cast a vote and return the Vote-only `AppMessage` to broadcast.
///
/// Always returns a `Vote` wire message — never a full `Proposal`. Every
/// peer already has the proposal registered in their session (either from
/// the initial unbundled broadcast or from the bundled-at-submit proposal
/// both land before anyone votes). Re-broadcasting the full proposal would
/// get rejected peer-side as `ProposalAlreadyExist`, dropping the vote.
///
/// The bundled-at-submit path in `register_new_proposal` calls
/// `consensus.cast_vote_and_get_proposal` directly — not this helper —
/// because that is the only legitimate case for broadcasting proposal +
/// vote atomically as a single wire message.
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
    let actor = if is_owner { "owner" } else { "member" };
    info!(group = group_name, proposal_id, choice, actor, "vote cast");

    let vote_msg = consensus
        .cast_vote(&scope, proposal_id, vote, signer)
        .await?;
    Ok(vote_msg.into())
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
/// Late-arrival classification uses the `Group::is_consensus_outcome_applied`
/// cache to tell benign late packets from suspicious unknowns:
/// - `SessionNotActive` — session exists but already resolved. Benign, debug.
/// - `SessionNotFound` with id in the resolved-proposals cache — session
///   was trimmed after local resolution. Benign, debug.
/// - `SessionNotFound` with id NOT in the cache — we never saw this
///   proposal. Suspicious (spurious packet or lost proposal). Warn-log,
///   swallow the error so inbound dispatch keeps draining.
///
/// Other consensus errors propagate.
pub async fn forward_incoming_vote<P: DeMlsProvider>(
    group_name: &str,
    group: &Group,
    vote: Vote,
    consensus: &ProviderConsensus<P>,
) -> Result<(), CoreError> {
    use hashgraph_like_consensus::error::ConsensusError;

    let proposal_id = vote.proposal_id;
    let scope = P::Scope::from(group_name.to_string());
    match consensus.process_incoming_vote(&scope, vote).await {
        Ok(()) => Ok(()),
        Err(ConsensusError::SessionNotActive) => {
            tracing::debug!(
                group = group_name,
                proposal_id,
                "late vote dropped: consensus session already resolved"
            );
            Ok(())
        }
        Err(ConsensusError::SessionNotFound) => {
            if group.is_consensus_outcome_applied(proposal_id) {
                tracing::debug!(
                    group = group_name,
                    proposal_id,
                    "late vote dropped: session trimmed after local resolution"
                );
            } else {
                tracing::warn!(
                    group = group_name,
                    proposal_id,
                    "vote for unknown proposal id dropped: no local session and not in resolved cache"
                );
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
