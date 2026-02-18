//! Consensus integration for DE-MLS group operations.
//!
//! This module bridges MLS group operations with the consensus voting layer.
//! It provides functions for creating proposals, casting votes, and handling
//! consensus outcomes.
//!
//! # Overview
//!
//! When a membership change is requested (join/remove), it goes through consensus:
//!
//! ```text
//! 1. Steward receives key package → ProcessResult::GetUpdateRequest
//! 2. dispatch_result() returns DispatchAction::StartVoting(request)
//! 3. App calls start_voting() → creates proposal, notifies UI
//! 4. Users vote via cast_vote() → votes sent as MLS messages
//! 5. Consensus service emits ConsensusEvent
//! 6. App calls handle_consensus_event() → updates proposal state
//! 7. Steward calls create_batch_proposals() → applies approved changes
//! ```
//!
//! # Key Functions
//!
//! ## Voting Workflow
//! - [`start_voting`] - Create a proposal and start voting
//! - [`cast_vote`] - Cast a vote on a proposal
//! - [`handle_consensus_event`] - Process consensus outcomes
//!
//! ## Message Forwarding
//! - [`forward_incoming_proposal`] - Forward received proposals to consensus
//! - [`forward_incoming_vote`] - Forward received votes to consensus
//!
//! ## Result Dispatching
//! - [`dispatch_result`] - Route [`ProcessResult`] to appropriate handlers
//!
//! # DispatchAction
//!
//! After calling [`dispatch_result`], handle the returned [`DispatchAction`]:
//!
//! - `Done` - Nothing more needed
//! - `StartVoting(request)` - Spawn a background task to run voting
//! - `GroupUpdated` - MLS state changed, update your state machine
//! - `LeaveGroup` - User was removed, clean up group state
//! - `JoinedGroup` - User joined successfully, transition to Working state

use alloy::signers::Signer;
use openmls_rust_crypto::MemoryStorage;
use prost::Message;
use std::time::Duration;
use tracing::info;

use hashgraph_like_consensus::{
    api::ConsensusServiceAPI,
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    types::{ConsensusEvent, CreateProposalRequest},
};

use crate::core::{
    CoreError, DeMlsProvider, GroupEventHandler, GroupHandle, ProcessResult, build_message,
};
use crate::mls_crypto::{DeMlsStorage, MlsService};
use crate::protos::de_mls::messages::v1::{
    AppMessage, ConversationMessage, GroupUpdateRequest, VotePayload, group_update_request,
};

/// Action returned by [`dispatch_result`] telling the caller what to do next.
///
/// This enum represents the application-level actions needed after processing
/// an inbound message. The core library handles protocol-level concerns;
/// these actions represent what your application layer needs to do.
#[derive(Debug)]
pub enum DispatchAction {
    /// Core handled the result fully; nothing more to do.
    ///
    /// The message was processed and any necessary callbacks were made.
    Done,

    /// A group update request needs consensus voting.
    ///
    /// The application should spawn a background task to:
    /// 1. Call [`start_voting`] with the request
    /// 2. Store the proposal ID via `GroupHandle::store_voting_proposal`
    StartVoting(GroupUpdateRequest),

    /// Group MLS state was updated (batch commit applied).
    ///
    /// The application should:
    /// - Transition state machine from Waiting → Working
    /// - Refresh UI with new group state
    GroupUpdated,

    /// The user was removed from the group.
    ///
    /// The application should:
    /// - Remove the group from its registry
    /// - Clean up any associated state
    /// - Notify the UI
    LeaveGroup,

    /// The user successfully joined a group.
    ///
    /// The application should:
    /// - Transition state machine from PendingJoin → Working
    /// - Start epoch timing synchronization
    JoinedGroup,
}

/// Create a consensus proposal for a group update request and start voting.
///
/// This function:
/// 1. Creates a `CreateProposalRequest` with the encoded group update
/// 2. Submits it to the consensus service
/// 3. Emits a `VotePayload` via the handler for UI display
///
/// # Arguments
/// * `group_name` - The name of the group
/// * `request` - The group update request (add/remove member)
/// * `expected_voters` - Number of group members (for quorum calculation)
/// * `identity_string` - The proposer's identity
/// * `consensus` - The consensus service
/// * `handler` - Event handler for UI notifications
///
/// # Returns
/// The proposal ID, which should be stored via `GroupHandle::store_voting_proposal`.
///
/// # Errors
/// - [`CoreError::ConsensusError`] if proposal creation fails
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
/// When a peer broadcasts their proposal, other members receive it as an
/// MLS application message. This function forwards it to the local consensus
/// service and emits a `VotePayload` so the UI can display the pending vote.
///
/// # Arguments
/// * `group_name` - The name of the group
/// * `proposal` - The received consensus proposal
/// * `consensus` - The consensus service
/// * `handler` - Event handler for UI notifications
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
/// When a peer casts their vote, it's broadcast as an MLS application message.
/// This function forwards it to the local consensus service for tallying.
///
/// # Arguments
/// * `group_name` - The name of the group
/// * `vote` - The received vote
/// * `consensus` - The consensus service
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
/// This function handles two cases:
/// - **Proposal owner**: Calls `cast_vote_and_get_proposal` and broadcasts
///   the full `Proposal` (so others can process it)
/// - **Non-owner**: Calls `cast_vote` and broadcasts just the `Vote`
///
/// # Arguments
/// * `handle` - The group handle (for proposal ownership check)
/// * `group_name` - The name of the group
/// * `proposal_id` - The proposal to vote on
/// * `vote` - true = approve, false = reject
/// * `consensus` - The consensus service
/// * `signer` - Ethereum signer for vote authentication
/// * `mls` - MLS service for message encryption
/// * `handler` - Event handler for sending the outbound message
///
/// # Errors
/// - [`CoreError::ConsensusError`] if voting fails
/// - [`CoreError::MlsServiceError`] if message encryption fails
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

/// Update handle state based on a consensus outcome event.
///
/// Called when the consensus service emits a [`ConsensusEvent`]. Updates
/// the group handle's proposal state based on the outcome:
///
/// - `ConsensusReached { result: true }` as owner → mark proposal as approved
/// - `ConsensusReached { result: true }` as non-owner → fetch and insert approved proposal
/// - `ConsensusReached { result: false }` → mark proposal as rejected
/// - `ConsensusFailed` → mark proposal as rejected
///
/// # Arguments
/// * `handle` - The group handle (will be mutated)
/// * `group_name` - The name of the group
/// * `event` - The consensus event to process
/// * `consensus` - The consensus service (for fetching proposal payloads)
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
                // Check if the just-approved proposal is an emergency criteria proposal.
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
            } else if !result && is_owner {
                handle.mark_proposal_as_rejected(proposal_id);
            } else if result && !is_owner {
                let scope = P::Scope::from(group_name.to_string());
                let payload = consensus.get_proposal_payload(&scope, proposal_id).await?;
                let update_request = GroupUpdateRequest::decode(payload.as_slice())?;

                if let Some(group_update_request::Payload::EmergencyCriteria(ref ec)) =
                    update_request.payload
                {
                    info!(
                        "Emergency criteria proposal {proposal_id} ACCEPTED for group {group_name}: \
                         violation_type={:?}. TODO (Milestone 2): apply peer score penalty to target.",
                        ec.evidence.as_ref().map(|e| e.violation_type)
                    );
                    // Emergency proposals don't produce MLS operations — don't add to approved.
                } else {
                    handle.insert_approved_proposal(proposal_id, update_request);
                }
            } else {
                // !result && !is_owner: proposal rejected
                // TODO (Milestone 2): If emergency criteria, apply false accusation penalty to creator.
                info!("Proposal {proposal_id} rejected (not owner, no local state to update)");
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

/// Dispatch a [`ProcessResult`] to the appropriate handlers.
///
/// This is the main routing function that connects [`process_inbound`](super::process_inbound)
/// results to consensus and event handlers. It returns a [`DispatchAction`]
/// telling your application what to do next.
///
/// # Arguments
/// * `handle` - The group handle
/// * `group_name` - The name of the group
/// * `result` - The result from `process_inbound`
/// * `consensus` - The consensus service
/// * `handler` - Event handler for callbacks
/// * `mls` - MLS service for message building
///
/// # Returns
/// A [`DispatchAction`] indicating what the application should do:
/// - `Done` - Nothing more needed
/// - `StartVoting(request)` - Spawn voting task
/// - `GroupUpdated` - Update state machine
/// - `LeaveGroup` - Clean up group
/// - `JoinedGroup` - Transition to Working state
pub async fn dispatch_result<P, S>(
    handle: &GroupHandle,
    group_name: &str,
    result: ProcessResult,
    consensus: &P::Consensus,
    handler: &dyn GroupEventHandler,
    mls: &MlsService<S>,
) -> Result<DispatchAction, CoreError>
where
    P: DeMlsProvider,
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
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
                message: format!("User {} joined the group", mls.wallet_hex()).into_bytes(),
                sender: "SYSTEM".to_string(),
                group_name: name.clone(),
            }
            .into();

            let packet = build_message(handle, mls, &msg)?;
            handler.on_outbound(&name, packet).await?;
            handler.on_joined_group(&name).await?;
            Ok(DispatchAction::JoinedGroup)
        }
        ProcessResult::ViolationDetected(evidence) => {
            info!(
                "Violation detected: type={}, target={:?}",
                evidence.violation_type, evidence.target_member_id
            );
            Ok(DispatchAction::StartVoting(evidence.into_update_request()))
        }
        ProcessResult::GroupUpdated => Ok(DispatchAction::GroupUpdated),
        ProcessResult::Noop => Ok(DispatchAction::Done),
    }
}
