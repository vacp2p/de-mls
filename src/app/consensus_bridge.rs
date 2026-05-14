//! App-layer adapters over the consensus service: proposal submission,
//! vote casting, and inbound forwarding for peer messages. These helpers
//! shape how consensus events surface in the UI and on the transport;
//! they are not protocol invariants.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hashgraph_like_consensus::{
    error::ConsensusError,
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    utils::build_vote,
};
use prost::Message;
use tracing::info;

use crate::{
    app::error::UserError,
    core::{
        ConsensusPlugin, Conversation, ConversationEventHandler, CoreError, PluginConsensus,
        build_create_proposal_request, self_leave_proposal_id,
    },
    protos::de_mls::messages::v1::{
        AppMessage, ConversationUpdateRequest, RemoveMember, VotePayload,
        conversation_update_request,
    },
};

/// Consensus-session parameters that come from `ConversationConfig`. Grouped so
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
/// In both cases the caller must record ownership *before*
/// casting, so a single-voter consensus transition can't fire before
/// `Conversation::is_owner_of_proposal` is true.
pub async fn submit_proposal<P: ConsensusPlugin>(
    conversation_name: &str,
    request: &ConversationUpdateRequest,
    creator_id: &[u8],
    consensus: &PluginConsensus<P>,
    params: ProposalParams,
) -> Result<(u32, AppMessage), CoreError> {
    let create_request = build_create_proposal_request(
        request,
        creator_id,
        params.expected_voters,
        params.proposal_expiration.as_secs(),
        params.liveness_criteria_yes,
    )?;

    let scope = P::Scope::from(conversation_name.to_string());
    let proposal = consensus
        .create_proposal_with_config(
            &scope,
            create_request,
            Some(ConsensusConfig::gossipsub().with_timeout(params.consensus_timeout)?),
        )
        .await?;

    info!(
        conversation = conversation_name,
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
/// rejected peer-side as `ProposalAlreadyExist`, dropping the vote.
///
/// The bundled-at-submit path in `register_new_proposal` calls
/// `consensus.cast_vote_and_get_proposal` directly rather than this
/// helper, because that is the only legitimate case for broadcasting
/// proposal + vote atomically in a single wire message.
pub async fn cast_vote<P>(
    conversation_name: &str,
    proposal_id: u32,
    vote: bool,
    consensus: &PluginConsensus<P>,
) -> Result<AppMessage, UserError>
where
    P: ConsensusPlugin,
{
    let scope = P::Scope::from(conversation_name.to_string());

    let choice = if vote { "YES" } else { "NO" };
    info!(
        conversation = conversation_name,
        proposal_id, choice, "vote cast"
    );

    let vote_msg = consensus.cast_vote(&scope, proposal_id, vote).await?;
    Ok(vote_msg.into())
}

/// Forward a peer's proposal into the local consensus service and, for
/// regular proposals, emit a `VotePayload` so the UI can surface the
/// pending vote. Fast-path proposals (`expected_voters_count == 1`)
/// self-resolve on arrival and skip the banner.
pub async fn relay_incoming_proposal<P: ConsensusPlugin>(
    conversation_name: &str,
    proposal: Proposal,
    consensus: &PluginConsensus<P>,
    handler: &dyn ConversationEventHandler,
) -> Result<(), UserError> {
    let scope = P::Scope::from(conversation_name.to_string());
    let expected_voters = proposal.expected_voters_count;
    consensus
        .process_incoming_proposal(&scope, proposal.clone())
        .await?;

    if expected_voters <= 1 {
        return Ok(());
    }

    let vote_notification: AppMessage = VotePayload {
        conversation_id: conversation_name.to_string(),
        proposal_id: proposal.proposal_id,
        payload: proposal.payload.clone(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs(),
    }
    .into();

    handler
        .on_app_message(conversation_name, vote_notification)
        .await?;
    Ok(())
}

/// Forward a peer's vote into the local consensus service.
///
/// Late-arrival classification uses the `Conversation::is_consensus_outcome_applied`
/// cache to tell benign late packets from suspicious unknowns:
/// - `SessionNotActive` — session exists but already resolved. Benign, debug.
/// - `SessionNotFound` with id in the resolved-proposals cache — session
///   was trimmed after local resolution. Benign, debug.
/// - `SessionNotFound` with id NOT in the cache — we never saw this
///   proposal. Suspicious (spurious packet or lost proposal). Warn-log,
///   swallow the error so inbound dispatch keeps draining.
///
/// Other consensus errors propagate.
pub async fn forward_incoming_vote<P: ConsensusPlugin>(
    conversation: &Conversation,
    vote: Vote,
    consensus: &PluginConsensus<P>,
) -> Result<(), CoreError> {
    let proposal_id = vote.proposal_id;
    let conversation_name = conversation.name();
    let scope = P::Scope::from(conversation_name.to_string());
    match consensus.process_incoming_vote(&scope, vote).await {
        Ok(()) => Ok(()),
        Err(ConsensusError::SessionNotActive) => {
            tracing::debug!(
                conversation = conversation_name,
                proposal_id,
                "late vote dropped: consensus session already resolved"
            );
            Ok(())
        }
        Err(ConsensusError::SessionNotFound) => {
            if conversation.is_consensus_outcome_applied(proposal_id) {
                tracing::debug!(
                    conversation = conversation_name,
                    proposal_id,
                    "late vote dropped: session trimmed after local resolution"
                );
            } else {
                tracing::warn!(
                    conversation = conversation_name,
                    proposal_id,
                    "vote for unknown proposal id dropped: no local session and not in resolved cache"
                );
            }
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Open a self-leave consensus session with the leaver's YES vote bundled
/// and `expected_voters_count = 1`.
///
/// Unlike [`submit_proposal`], this hand-crafts the `Proposal` so it carries
/// the deterministic `self_leave_proposal_id(identity)`. Every node
/// derives the same id from the MLS-authenticated sender, so a
/// retransmitted self-leave dedupes natively via `ProposalAlreadyExist` and
/// every node's `approved_proposals` entry ends up under the same key.
///
/// On the leaver's side the bundled YES vote (+ `expected_voters=1`) makes
/// `from_proposal` fire `ConsensusReached(true)` synchronously, so the
/// normal `apply_consensus_outcome` path can place `RemoveMember(self)` in
/// `approved_proposals` with the deterministic id.
///
/// Returns `Ok(Some(...))` if the proposal was newly opened (caller must
/// broadcast the `AppMessage`). Returns `Ok(None)` if the session already
/// exists (e.g. the user double-clicked Leave and the dedup fired) — no
/// broadcast needed. Other consensus errors propagate.
pub async fn submit_self_leave_proposal<P>(
    conversation_name: &str,
    self_identity: &[u8],
    consensus: &PluginConsensus<P>,
    params: ProposalParams,
) -> Result<Option<(u32, AppMessage)>, UserError>
where
    P: ConsensusPlugin,
{
    let request = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::RemoveMember(
            RemoveMember {
                identity: self_identity.to_vec(),
            },
        )),
    };
    let payload = request.encode_to_vec();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(CoreError::from)?
        .as_secs();
    let expiration = now.saturating_add(params.proposal_expiration.as_secs());

    let proposal_id = self_leave_proposal_id(self_identity);
    let mut proposal = Proposal {
        name: format!("self-leave:{proposal_id}"),
        payload,
        proposal_id,
        proposal_owner: self_identity.to_vec(),
        votes: Vec::new(),
        expected_voters_count: params.expected_voters,
        round: 1,
        timestamp: now,
        expiration_timestamp: expiration,
        liveness_criteria_yes: params.liveness_criteria_yes,
    };

    let yes_vote = build_vote(&proposal, true, consensus.signer())
        .await
        .map_err(CoreError::from)?;
    proposal.votes.push(yes_vote);

    let scope = P::Scope::from(conversation_name.to_string());
    match consensus
        .process_incoming_proposal(&scope, proposal.clone())
        .await
    {
        Ok(()) => {
            info!(
                conversation = conversation_name,
                proposal_id, "self-leave proposal opened (expected_voters=1, bundled YES)"
            );
            Ok(Some((proposal_id, proposal.into())))
        }
        Err(ConsensusError::ProposalAlreadyExist) => {
            info!(
                conversation = conversation_name,
                proposal_id, "self-leave already in flight, skipping retransmit"
            );
            Ok(None)
        }
        Err(e) => Err(CoreError::from(e).into()),
    }
}
