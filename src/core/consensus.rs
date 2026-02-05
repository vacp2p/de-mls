//! Consensus integration for DE-MLS group operations.
//!
//! This module provides free functions that wire MLS group operations
//! to the consensus voting layer. External developers can use these
//! directly without depending on the `app` layer.

use std::time::Duration;

use alloy::signers::Signer;
use hashgraph_like_consensus::api::ConsensusServiceAPI;
use hashgraph_like_consensus::protos::consensus::v1::{Proposal, Vote};
use hashgraph_like_consensus::session::ConsensusConfig;
use hashgraph_like_consensus::types::{ConsensusEvent, CreateProposalRequest};
use mls_crypto::IdentityService;
use prost::Message;
use tracing::info;

use crate::core::{build_message, CoreError, DeMlsProvider, GroupEventHandler, GroupHandle};
use crate::protos::de_mls::messages::v1::{
    AppMessage, ConversationMessage, GroupUpdateRequest, VotePayload,
};

use super::ProcessResult;

/// Action returned by [`dispatch_result`] telling the caller what to do next.
#[derive(Debug)]
pub enum DispatchAction {
    /// Core handled the result fully; nothing more to do.
    Done,
    /// A group update request needs consensus voting (app should spawn background task).
    StartVoting(GroupUpdateRequest),
    /// Group MLS state was updated (batch applied — app state machine should advance).
    GroupUpdated,
    /// The user was removed from the group (app should clean up).
    LeaveGroup,
    /// The user successfully joined a group.
    JoinedGroup,
}

/// Create a consensus proposal for a group update request and start voting.
///
/// Creates a `CreateProposalRequest`, submits it to the consensus service,
/// and emits a `VotePayload` via the handler. Returns the proposal ID so
/// the caller can store the voting proposal in the handle (via
/// [`GroupHandle::store_voting_proposal`]).
///
/// Callers should obtain `expected_voters` via `group_members` and `identity_string`
/// via [`IdentityService::identity_string`] before calling this function. This design
/// allows the function to be used from spawned tasks without requiring the identity service.
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
/// After forwarding, emits a `VotePayload` to the handler so the UI
/// can display the pending vote.
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
pub async fn forward_incoming_vote<P: DeMlsProvider>(
    group_name: &str,
    vote: Vote,
    consensus: &P::Consensus,
) -> Result<(), CoreError> {
    let scope = P::Scope::from(group_name.to_string());
    consensus.process_incoming_vote(&scope, vote).await?;
    Ok(())
}

/// Cast a vote on a proposal and send the result as an outbound MLS message.
///
/// If the caller is the proposal owner, calls `cast_vote_and_get_proposal`
/// and sends the full `Proposal` as an outbound MLS message.
/// Otherwise, calls `cast_vote` and sends the `Vote`.
#[allow(clippy::too_many_arguments)]
pub async fn cast_vote<P: DeMlsProvider, SN: Signer + Send + Sync>(
    handle: &GroupHandle,
    group_name: &str,
    proposal_id: u32,
    vote: bool,
    consensus: &P::Consensus,
    signer: SN,
    identity: &P::Identity,
    handler: &dyn GroupEventHandler,
) -> Result<(), CoreError> {
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

    let packet = build_message(handle, identity, &app_message).await?;
    handler.on_outbound(group_name, packet).await?;
    Ok(())
}

/// Update handle state based on a consensus outcome event.
///
/// - `ConsensusReached { result: true }` as owner → mark proposal as approved
/// - `ConsensusReached { result: true }` as non-owner → fetch payload and insert approved proposal
/// - `ConsensusReached { result: false }` or `ConsensusFailed` → mark proposal as rejected
pub async fn handle_consensus_event<P: DeMlsProvider>(
    handle: &mut GroupHandle,
    group_name: &str,
    event: ConsensusEvent,
    consensus: &P::Consensus,
) -> Result<(), CoreError> {
    match event {
        ConsensusEvent::ConsensusReached {
            proposal_id,
            result,
            timestamp: _,
        } => {
            info!("Consensus reached for proposal {proposal_id}: result={result}");
            let is_owner = handle.is_owner_of_proposal(proposal_id);
            if result && is_owner {
                handle.mark_proposal_as_approved(proposal_id);
            } else if !result && is_owner {
                handle.mark_proposal_as_rejected(proposal_id);
            } else if result && !is_owner {
                let scope = P::Scope::from(group_name.to_string());
                let payload = consensus.get_proposal_payload(&scope, proposal_id).await?;
                let update_request = GroupUpdateRequest::decode(payload.as_slice())?;
                handle.insert_approved_proposal(proposal_id, update_request);
            }
        }
        ConsensusEvent::ConsensusFailed {
            proposal_id,
            timestamp: _,
        } => {
            info!("Consensus failed for proposal {proposal_id}");
            handle.mark_proposal_as_rejected(proposal_id);
        }
    }

    Ok(())
}

/// Dispatch a [`ProcessResult`] using the consensus functions above.
///
/// Returns a [`DispatchAction`] telling the caller what further action is needed:
/// - `Done` — core handled everything
/// - `StartVoting(request)` — app should spawn a background voting task
/// - `GroupUpdated` — app state machine should advance
pub async fn dispatch_result<P: DeMlsProvider>(
    handle: &GroupHandle,
    group_name: &str,
    result: ProcessResult,
    consensus: &P::Consensus,
    handler: &dyn GroupEventHandler,
    identity: &P::Identity,
) -> Result<DispatchAction, CoreError> {
    match result {
        ProcessResult::AppMessage(msg) => {
            handler.on_app_message(group_name, msg).await?;
            Ok(DispatchAction::Done)
        }
        ProcessResult::LeaveGroup => Ok(DispatchAction::LeaveGroup),
        ProcessResult::Proposal(proposal) => {
            forward_incoming_proposal::<P>(group_name, proposal, consensus, handler).await?;
            Ok(DispatchAction::Done)
        }
        ProcessResult::Vote(vote) => {
            forward_incoming_vote::<P>(group_name, vote, consensus).await?;
            Ok(DispatchAction::Done)
        }
        ProcessResult::GetUpdateRequest(request) => Ok(DispatchAction::StartVoting(request)),
        ProcessResult::JoinedGroup(name) => {
            let msg: AppMessage = ConversationMessage {
                message: format!("User {} joined the group", identity.identity_string())
                    .into_bytes(),
                sender: "SYSTEM".to_string(),
                group_name: name.clone(),
            }
            .into();

            let packet = build_message(handle, identity, &msg).await?;
            handler.on_outbound(&name, packet).await?;
            handler.on_joined_group(&name).await?;
            Ok(DispatchAction::JoinedGroup)
        }
        ProcessResult::GroupUpdated => Ok(DispatchAction::GroupUpdated),
        ProcessResult::Noop => Ok(DispatchAction::Done),
    }
}
