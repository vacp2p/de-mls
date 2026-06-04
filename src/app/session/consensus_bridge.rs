//! App-layer adapters over the consensus service: proposal submission,
//! vote casting, and inbound forwarding for peer messages. These helpers
//! shape how consensus events surface in the UI and on the transport;

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
    app::error::UserError,
    core::{ConsensusPlugin, ConsensusServiceFor, CoreError, self_leave_proposal_id},
    protos::de_mls::messages::v1::{
        AppMessage, ConversationUpdateRequest, RemoveMember, conversation_update_request,
    },
};

/// Consensus-session parameters that come from `ConversationConfig`. Grouped so
/// [`submit_proposal`]'s argument list stays readable.
pub(crate) struct ProposalParams {
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
/// In both cases the caller must record ownership *before*
/// casting, so a single-voter consensus transition can't fire before
/// [`crate::core::ConversationQueues::is_owner_of_proposal`] is true.
pub(crate) fn submit_proposal<P: ConsensusPlugin>(
    conversation_id: &str,
    request: &ConversationUpdateRequest,
    creator_id: &[u8],
    consensus: &ConsensusServiceFor<P>,
    params: ProposalParams,
) -> Result<(u32, AppMessage), CoreError> {
    let create_request = CreateProposalRequest::new(
        uuid::Uuid::new_v4().to_string(),
        request.encode_to_vec(),
        creator_id.to_vec(),
        params.expected_voters,
        params.proposal_expiration.as_secs(),
        params.liveness_criteria_yes,
    )?;

    let scope = P::Scope::from(conversation_id.to_string());
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

/// Cast a vote and return the Vote-only `AppMessage` to broadcast.
///
/// Always returns a `Vote` wire message — never a full `Proposal`. Every
/// peer already has the proposal registered in their session (either from
/// the unbundled broadcast or from a bundled-at-submit proposal — both
/// land before anyone votes). Re-broadcasting the full proposal would be
/// rejected peer-side as [`ConsensusError::ProposalAlreadyExist`], dropping the vote.
///
/// The bundled-at-submit path in `register_new_proposal` calls
/// `consensus.cast_vote_and_get_proposal` directly rather than this
/// helper, because that is the only legitimate case for broadcasting
/// proposal + vote atomically in a single wire message.
pub(crate) fn cast_vote<P>(
    conversation_id: &str,
    proposal_id: u32,
    vote: bool,
    consensus: &ConsensusServiceFor<P>,
) -> Result<AppMessage, UserError>
where
    P: ConsensusPlugin,
{
    let scope = P::Scope::from(conversation_id.to_string());

    let choice = if vote { "YES" } else { "NO" };
    info!(
        conversation = conversation_id,
        proposal_id, choice, "vote cast"
    );

    let vote_msg = consensus.cast_vote(&scope, proposal_id, vote)?;
    Ok(vote_msg.into())
}

/// Forward a peer's proposal into the local consensus service. The caller
/// decides whether to emit a banner event — for fast-path proposals
/// (`expected_voters_count == 1`) the session resolves on arrival, so
/// there's nothing to vote on.
pub(crate) fn forward_incoming_proposal<P: ConsensusPlugin>(
    conversation_id: &str,
    proposal: Proposal,
    consensus: &ConsensusServiceFor<P>,
) -> Result<(), UserError> {
    let scope = P::Scope::from(conversation_id.to_string());
    consensus.process_incoming_proposal(&scope, proposal)?;
    Ok(())
}

/// Forward a peer's vote into the local consensus service.
///
/// `outcome_applied_locally` is the result of
/// `ConversationQueues::is_consensus_outcome_applied(vote.proposal_id)` computed
/// by the caller — passed in eagerly so this helper doesn't have to borrow
/// the conversation across the consensus-service call. It's only consulted on
/// the `SessionNotFound` branch.
///
/// Late-arrival classification:
/// - `SessionNotActive` — session exists but already resolved. Benign, debug.
/// - `SessionNotFound` with `outcome_applied_locally = true` — session
///   was trimmed after local resolution. Benign, debug.
/// - `SessionNotFound` with `outcome_applied_locally = false` — we never
///   saw this proposal. Suspicious (spurious packet or lost proposal).
///   Warn-log, swallow the error so inbound dispatch keeps draining.
///
/// Other consensus errors propagate.
pub(crate) fn forward_incoming_vote<P: ConsensusPlugin>(
    conversation_id: &str,
    vote: Vote,
    consensus: &ConsensusServiceFor<P>,
    outcome_applied_locally: bool,
) -> Result<(), CoreError> {
    let proposal_id = vote.proposal_id;
    let scope = P::Scope::from(conversation_id.to_string());
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

/// Open a self-leave consensus session: `expected_voters_count = 1` with
/// the leaver's YES bundled, so it resolves synchronously and the normal
/// `apply_consensus_outcome` path commits `RemoveMember(self)`.
///
/// Unlike [`submit_proposal`], the `Proposal` is hand-crafted to carry
/// `self_leave_proposal_id(member_id)`. Every node derives the same id, so a
/// retransmitted self-leave dedupes via `ProposalAlreadyExist` and lands in
/// every `approved_proposals` under the same key.
///
/// `Ok(Some(_))` — newly opened; caller broadcasts the `AppMessage`.
/// `Ok(None)` — already in flight (e.g. double-click); no broadcast.
pub(crate) fn submit_self_leave_proposal<P>(
    conversation_id: &str,
    self_member_id: &[u8],
    consensus: &ConsensusServiceFor<P>,
    params: ProposalParams,
) -> Result<Option<(u32, AppMessage)>, UserError>
where
    P: ConsensusPlugin,
{
    let request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember {
                member_id: self_member_id.to_vec(),
            },
        )),
    };
    let payload = request.encode_to_vec();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(CoreError::from)?
        .as_secs();
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

    let scope = P::Scope::from(conversation_id.to_string());
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
