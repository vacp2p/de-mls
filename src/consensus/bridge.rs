//! Stateless adapters between conversation intents and the consensus library.
//!
//! Every function here talks only to the consensus service, never to
//! conversation state; callers encrypt and broadcast any returned wire message.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hashgraph_like_consensus::{
    error::ConsensusError,
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    types::CreateProposalRequest,
    utils::build_vote,
};
use prost::Message;
use tracing::info;

use crate::{
    ConsensusPlugin, ConsensusServiceFor, ConversationError,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest},
    self_leave_proposal_id,
};

/// Per-proposal consensus-session parameters.
pub(crate) struct ProposalParams {
    pub expected_voters: u32,
    pub proposal_expiration: Duration,
    pub consensus_timeout: Duration,
    pub liveness_criteria_yes: bool,
}

/// Open a consensus session for `ConversationUpdateRequest`; returns the new `proposal_id`
/// and the unbundled `Proposal` wire message.
pub(crate) fn submit_proposal<C: ConsensusPlugin>(
    conversation_id: &str,
    request: &ConversationUpdateRequest,
    creator_id: &[u8],
    consensus: &ConsensusServiceFor<C>,
    params: ProposalParams,
) -> Result<(u32, AppMessage), ConversationError> {
    let create_request = CreateProposalRequest::new(
        uuid::Uuid::new_v4().to_string(),
        request.encode_to_vec(),
        creator_id.to_vec(),
        params.expected_voters,
        params.proposal_expiration.as_secs(),
        params.liveness_criteria_yes,
    )?;

    let scope = C::Scope::from(conversation_id.to_string());
    let proposal = consensus.create_proposal_with_config(
        &scope,
        create_request,
        Some(ConsensusConfig::gossipsub().with_timeout(params.consensus_timeout)?),
    )?;

    info!(
        conversation = conversation_id,
        proposal_id = proposal.proposal_id,
        voters = params.expected_voters,
        "proposal opened"
    );

    let proposal_id = proposal.proposal_id;
    Ok((proposal_id, proposal.into()))
}

/// Cast our vote in the local session; returns the Vote-only wire message.
pub(crate) fn cast_vote<C>(
    conversation_id: &str,
    proposal_id: u32,
    vote: bool,
    consensus: &ConsensusServiceFor<C>,
) -> Result<AppMessage, ConversationError>
where
    C: ConsensusPlugin,
{
    let scope = C::Scope::from(conversation_id.to_string());

    let choice = if vote { "YES" } else { "NO" };
    info!(
        conversation = conversation_id,
        proposal_id, choice, "vote cast"
    );

    let vote_msg = consensus.cast_vote(&scope, proposal_id, vote)?;
    Ok(vote_msg.into())
}

/// Feed a peer's vote into the local consensus session.
///
/// Late arrivals are swallowed (logged, `Ok`) so inbound dispatch keeps
/// draining.
pub(crate) fn forward_incoming_vote<C: ConsensusPlugin>(
    conversation_id: &str,
    vote: Vote,
    consensus: &ConsensusServiceFor<C>,
    outcome_applied_locally: bool,
) -> Result<(), ConversationError> {
    let proposal_id = vote.proposal_id;
    let scope = C::Scope::from(conversation_id.to_string());
    match consensus.process_incoming_vote(&scope, vote) {
        Ok(()) => Ok(()),
        Err(ConsensusError::SessionNotActive) => {
            tracing::debug!(
                conversation = conversation_id,
                proposal_id,
                "late vote dropped: consensus session already resolved"
            );
            Ok(())
        }
        Err(ConsensusError::SessionNotFound) => {
            if outcome_applied_locally {
                tracing::debug!(
                    conversation = conversation_id,
                    proposal_id,
                    "late vote dropped: session trimmed after local resolution"
                );
            } else {
                tracing::warn!(
                    conversation = conversation_id,
                    proposal_id,
                    "vote for unknown proposal id dropped: no local session and not in resolved cache"
                );
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Open a self-leave session: a hand-crafted `Proposal` carrying the
/// deterministic [`self_leave_proposal_id`] and the leaver's signed YES,
/// so the single-voter session resolves on arrival.
///
/// The fixed id makes retransmits collide as `ProposalAlreadyExist`,
/// returned as `Ok(None)` — nothing to broadcast.
pub(crate) fn submit_self_leave_proposal<C>(
    conversation_id: &str,
    self_member_id: &[u8],
    consensus: &ConsensusServiceFor<C>,
    params: ProposalParams,
) -> Result<Option<(u32, AppMessage)>, ConversationError>
where
    C: ConsensusPlugin,
{
    let request = ConversationUpdateRequest::remove_member(self_member_id.to_vec());
    let payload = request.encode_to_vec();

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let expiration = now.saturating_add(params.proposal_expiration.as_secs());

    let proposal_id = self_leave_proposal_id(self_member_id);
    let mut proposal = Proposal {
        name: format!("self-leave:{proposal_id}"),
        payload,
        proposal_id,
        proposal_owner: self_member_id.to_vec(),
        votes: Vec::new(),
        expected_voters_count: params.expected_voters,
        round: 1,
        timestamp: now,
        expiration_timestamp: expiration,
        liveness_criteria_yes: params.liveness_criteria_yes,
    };

    let yes_vote = build_vote(&proposal, true, consensus.signer())?;
    proposal.votes.push(yes_vote);

    let scope = C::Scope::from(conversation_id.to_string());
    match consensus.process_incoming_proposal(&scope, proposal.clone()) {
        Ok(()) => {
            info!(
                conversation = conversation_id,
                proposal_id, "self-leave proposal opened (expected_voters=1, bundled YES)"
            );
            Ok(Some((proposal_id, proposal.into())))
        }
        Err(ConsensusError::ProposalAlreadyExist) => {
            info!(
                conversation = conversation_id,
                proposal_id, "self-leave already in flight, skipping retransmit"
            );
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}
