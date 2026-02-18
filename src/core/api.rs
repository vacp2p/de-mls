//! Core API for MLS group operations.
//!
//! This module provides the fundamental building blocks for MLS group management.
//! All functions operate on [`GroupHandle`] instances for app-level state and
//! [`MlsService`] for MLS cryptographic operations.
//!
//! # Overview
//!
//! The API is organized into four categories:
//!
//! ## Group Lifecycle
//! - [`create_group`] - Create a new group as steward
//! - [`prepare_to_join`] - Prepare a handle for joining
//! - [`join_group_from_invite`] - Complete join with welcome message
//! - [`become_steward`] / [`resign_steward`] - Steward role management
//!
//! ## Message Operations
//! - [`build_message`] - Encrypt an application message for the group
//! - [`build_key_package_message`] - Create key package for joining
//!
//! ## Inbound Processing
//! - [`process_inbound`] - Process received packets, returns [`ProcessResult`]
//!
//! ## Steward Operations
//! - [`create_batch_proposals`] - Batch approved proposals into MLS commit
//!
//! # Typical Flow
//!
//! ```text
//! Steward creates group:
//!   mls.create_group(name) → mls state initialized
//!   create_group(name) → GroupHandle (steward=true)
//!
//! Member joins:
//!   prepare_to_join() → GroupHandle (steward=false)
//!   build_key_package_message() → send to network
//!   ... wait for welcome ...
//!   process_inbound(welcome) → ProcessResult::JoinedGroup
//!
//! Sending messages:
//!   build_message(app_msg) → OutboundPacket → send to network
//!
//! Receiving messages:
//!   process_inbound(payload) → ProcessResult → dispatch_result() → DispatchAction
//! ```

use openmls_rust_crypto::MemoryStorage;
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use tracing::info;

use crate::core::{
    ProposalId, error::CoreError, group_handle::GroupHandle, types::ProcessResult,
    types::invitation_from_bytes,
};
use crate::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use crate::mls_crypto::{
    CommitResult, DeMlsStorage, DecryptResult, GroupUpdate, KeyPackageBytes, MlsService,
    key_package_bytes_from_json,
};
use crate::protos::de_mls::messages::v1::{
    AppMessage, BatchProposalsMessage, GroupUpdateRequest, InviteMember, UserKeyPackage,
    ViolationEvidence, WelcomeMessage, app_message, group_update_request, welcome_message,
};

// ─────────────────────────── Group Lifecycle ───────────────────────────

/// Create a new MLS group as the steward.
///
/// The creator automatically becomes the steward (the member responsible for
/// batching proposals and sending commits).
///
/// # Arguments
/// * `name` - The name/identifier of the group
/// * `mls` - The MLS service for cryptographic operations
///
/// # Returns
/// A [`GroupHandle`] with steward=true.
///
/// # Errors
/// Returns [`CoreError::MlsServiceError`] if group creation fails.
pub fn create_group<S>(name: &str, mls: &MlsService<S>) -> Result<GroupHandle, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    mls.create_group(name)?;
    Ok(GroupHandle::new_as_creator(name, mls.wallet_bytes()))
}

/// Prepare a handle for joining an existing group.
///
/// Creates a [`GroupHandle`] without MLS state. The MLS state will be
/// initialized when a welcome message is received and processed.
///
/// # Arguments
/// * `name` - The name of the group to join
///
/// # Returns
/// A [`GroupHandle`] ready for joining (steward=false, MLS not initialized).
///
/// # Next Steps
/// 1. Call [`build_key_package_message`] to create a join request
/// 2. Send the key package to the network
/// 3. Wait for [`ProcessResult::JoinedGroup`] from [`process_inbound`]
pub fn prepare_to_join(name: &str) -> GroupHandle {
    GroupHandle::new_for_join(name)
}

/// Complete joining a group using a welcome message.
///
/// This is typically called internally by [`process_inbound`] when a welcome
/// is received. You can also call it directly if you have the raw welcome bytes.
///
/// # Arguments
/// * `handle` - The group handle (must be prepared with [`prepare_to_join`])
/// * `welcome_bytes` - The serialized MLS welcome message
/// * `mls` - The MLS service for processing the welcome
///
/// # Returns
/// The group name extracted from the welcome message.
///
/// # Errors
/// Returns [`CoreError::MlsServiceError`] if welcome processing fails.
pub fn join_group_from_invite<S>(
    handle: &mut GroupHandle,
    welcome_bytes: &[u8],
    mls: &MlsService<S>,
) -> Result<String, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = mls.join_group(welcome_bytes)?;
    handle.set_mls_initialized();
    Ok(group_name)
}

/// Become the steward of a group.
///
/// The steward is responsible for:
/// - Receiving key packages from joining members
/// - Batching approved proposals into MLS commits
/// - Sending welcome messages to new members
///
/// # Note
/// Only one member should be steward at a time. Steward election/handoff
/// is managed at the application layer.
pub fn become_steward(handle: &mut GroupHandle) {
    handle.become_steward();
}

/// Resign as steward of a group.
///
/// After resigning, another member should become steward to continue
/// processing membership changes.
pub fn resign_steward(handle: &mut GroupHandle) {
    handle.resign_steward();
}

// ─────────────────────────── Message Building ───────────────────────────

/// Build an MLS-encrypted application message.
///
/// Encrypts the given [`AppMessage`] using MLS and wraps it in an
/// [`OutboundPacket`] ready for network transmission.
///
/// # Arguments
/// * `handle` - The group handle (for routing metadata)
/// * `mls` - The MLS service for encryption
/// * `app_msg` - The application message to encrypt
///
/// # Returns
/// An [`OutboundPacket`] containing the encrypted message.
///
/// # Errors
/// - [`CoreError::MlsGroupNotInitialized`] if MLS state is not initialized
/// - [`CoreError::MlsServiceError`] if encryption fails
pub fn build_message<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
    app_msg: &AppMessage,
) -> Result<OutboundPacket, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_mls_initialized() {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    let message_out = mls.encrypt(handle.group_name(), &app_msg.encode_to_vec())?;

    Ok(OutboundPacket::new(
        message_out,
        APP_MSG_SUBTOPIC,
        handle.group_name(),
        handle.app_id(),
    ))
}

/// Build a key package message for joining a group.
///
/// Generates a new MLS key package and wraps it in a [`WelcomeMessage`]
/// for transmission to the group's steward.
///
/// # Arguments
/// * `handle` - The group handle (for routing metadata)
/// * `mls` - The MLS service for key package generation
///
/// # Returns
/// An [`OutboundPacket`] containing the key package on the welcome subtopic.
///
/// # Errors
/// Returns [`CoreError::MlsServiceError`] if key package generation fails.
pub fn build_key_package_message<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
) -> Result<OutboundPacket, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let key_package = mls.generate_key_package()?;
    let welcome_msg: WelcomeMessage = UserKeyPackage {
        key_package_bytes: key_package.as_bytes().to_vec(),
    }
    .into();

    Ok(OutboundPacket::new(
        welcome_msg.encode_to_vec(),
        WELCOME_SUBTOPIC,
        handle.group_name(),
        handle.app_id(),
    ))
}

// ─────────────────────────── Inbound Processing ───────────────────────────

/// Process an inbound packet and determine what action is needed.
///
/// This is the main entry point for handling received messages. It routes
/// packets based on subtopic and returns a [`ProcessResult`] indicating
/// what happened.
///
/// # Arguments
/// * `handle` - The group handle (may be mutated if joining)
/// * `payload` - The raw packet payload
/// * `subtopic` - The subtopic the packet was received on
/// * `mls` - The MLS service for decryption and welcome processing
///
/// # Returns
/// A [`ProcessResult`] indicating what happened:
/// - `AppMessage` - A chat message was received
/// - `Proposal` / `Vote` - Consensus message received
/// - `GetUpdateRequest` - Steward received a join/remove request
/// - `JoinedGroup` - Successfully joined via welcome
/// - `GroupUpdated` - MLS state was updated (batch commit applied)
/// - `LeaveGroup` - User was removed from the group
/// - `Noop` - Nothing to do (message not for us, etc.)
///
/// # Errors
/// - [`CoreError::InvalidSubtopic`] if subtopic is unknown
/// - [`CoreError::MlsServiceError`] if decryption fails
/// - [`CoreError::MessageError`] if message parsing fails
///
/// # Next Steps
/// Pass the result to [`dispatch_result`](super::dispatch_result) to handle
/// consensus routing and get a [`DispatchAction`](super::DispatchAction).
pub fn process_inbound<S>(
    handle: &mut GroupHandle,
    payload: &[u8],
    subtopic: &str,
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    match subtopic {
        WELCOME_SUBTOPIC => process_welcome_subtopic(handle, payload, mls),
        APP_MSG_SUBTOPIC => process_app_subtopic(handle, payload, mls),
        _ => Err(CoreError::InvalidSubtopic(subtopic.to_string())),
    }
}

fn process_welcome_subtopic<S>(
    handle: &mut GroupHandle,
    payload: &[u8],
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let welcome_msg = WelcomeMessage::decode(payload)?;
    match welcome_msg.payload {
        Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
            if handle.is_steward() {
                info!(
                    "Steward received key package for group {}",
                    handle.group_name()
                );
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;

                let gur = GroupUpdateRequest {
                    payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                        key_package_bytes,
                        identity,
                    })),
                };

                return Ok(ProcessResult::GetUpdateRequest(gur));
            }
            Ok(ProcessResult::Noop)
        }
        Some(welcome_message::Payload::InvitationToJoin(invitation)) => {
            // Stewards and already-joined members ignore invitations
            if handle.is_steward() || handle.is_mls_initialized() {
                return Ok(ProcessResult::Noop);
            }

            // Check if this invitation is for us
            if mls.is_welcome_for_us(&invitation.mls_message_out_bytes)? {
                let group_name = mls.join_group(&invitation.mls_message_out_bytes)?;
                handle.set_mls_initialized();
                info!(
                    "[process_welcome_subtopic]: Joined group {}",
                    handle.group_name()
                );
                return Ok(ProcessResult::JoinedGroup(group_name));
            }
            Ok(ProcessResult::Noop)
        }
        None => Ok(ProcessResult::Noop),
    }
}

fn process_app_subtopic<S>(
    handle: &mut GroupHandle,
    payload: &[u8],
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_mls_initialized() {
        return Ok(ProcessResult::Noop);
    }

    // Try to parse as AppMessage first (for batch proposals)
    if let Ok(app_message) = AppMessage::decode(payload)
        && let Some(app_message::Payload::BatchProposalsMessage(batch_msg)) = app_message.payload
    {
        return process_batch_proposals(handle, batch_msg, mls);
    }

    // Fall back to MLS protocol message
    let res = mls.decrypt(handle.group_name(), payload)?;

    match res {
        DecryptResult::Application(app_bytes) => AppMessage::decode(app_bytes.as_ref())?.try_into(),
        DecryptResult::Removed => Ok(ProcessResult::LeaveGroup),
        DecryptResult::ProposalStored | DecryptResult::CommitProcessed | DecryptResult::Ignored => {
            Ok(ProcessResult::Noop)
        }
    }
}

/// Compute a canonical SHA-256 digest over a set of approved proposals.
///
/// Proposals are sorted by ID to ensure deterministic ordering, then each
/// `(id, serialized_payload)` pair is fed into the hash. Both the steward
/// (when building the batch) and receivers (when validating) call this with
/// the same `HashMap`, so the digest must match if and only if the proposal
/// content is identical.
fn compute_proposals_digest(proposals: &HashMap<ProposalId, GroupUpdateRequest>) -> Vec<u8> {
    let sorted: BTreeMap<_, _> = proposals.iter().collect();
    let mut hasher = Sha256::new();
    for (&id, req) in &sorted {
        hasher.update(id.to_le_bytes());
        hasher.update(req.encode_to_vec());
    }
    hasher.finalize().to_vec()
}

fn process_batch_proposals<S>(
    handle: &mut GroupHandle,
    batch_msg: BatchProposalsMessage,
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = handle.group_name().to_owned();
    let local_proposals = handle.approved_proposals();
    // The target of any violation is the steward who sent this batch.
    let steward_id = handle.steward_identity().unwrap_or_default().to_vec();

    // ── 1. Verify proposal-set IDs match (fast reject) ─────────
    let local_ids: BTreeSet<ProposalId> = local_proposals.keys().copied().collect();
    let batch_ids: BTreeSet<ProposalId> = batch_msg.proposal_ids.iter().copied().collect();

    if local_ids != batch_ids {
        tracing::warn!(
            "Violation: broken commit for group {} — proposal ID set mismatch \
             (local={:?}, batch={:?})",
            group_name,
            local_ids,
            batch_ids,
        );
        // BROKEN_COMMIT: ID mismatch means the steward included different proposals
        // than what was voted on. This is malicious behaviour.
        return Ok(ProcessResult::ViolationDetected(
            ViolationEvidence::broken_commit(
                steward_id.clone(),
                handle.current_epoch(),
                format!("proposal ID mismatch: local={local_ids:?}, batch={batch_ids:?}"),
            ),
        ));
    }

    if local_ids.is_empty() {
        tracing::warn!(
            "Rejecting batch for group {}: no approved proposals",
            group_name,
        );
        return Ok(ProcessResult::Noop);
    }

    // ── 2. Verify content digest (cryptographic binding) ───────
    let local_digest = compute_proposals_digest(&local_proposals);
    if batch_msg.proposals_digest != local_digest {
        tracing::warn!(
            "Violation: broken commit for group {} — IDs match but \
             digest mismatch (steward altered proposal content)",
            group_name,
        );
        // BROKEN_COMMIT: IDs match but digest differs — steward altered proposal content.
        return Ok(ProcessResult::ViolationDetected(
            ViolationEvidence::broken_commit(
                steward_id.clone(),
                handle.current_epoch(),
                batch_msg.proposals_digest,
            ),
        ));
    }

    // ── 3. Proposal count must match MLS payloads ──────────────
    if batch_msg.mls_proposals.len() != local_ids.len() {
        tracing::warn!(
            "Violation: broken MLS proposal for group {} — proposal count ({}) \
             does not match MLS payload count ({})",
            group_name,
            local_ids.len(),
            batch_msg.mls_proposals.len(),
        );
        // BROKEN_MLS_PROPOSAL: digest matches but MLS payload count is wrong.
        return Ok(ProcessResult::ViolationDetected(
            ViolationEvidence::broken_mls_proposal(
                steward_id.clone(),
                handle.current_epoch(),
                format!(
                    "expected {} MLS proposals, got {}",
                    local_ids.len(),
                    batch_msg.mls_proposals.len()
                ),
            ),
        ));
    }

    // ── 4. Decrypt each proposal, enforce ProposalStored ───────
    for (i, proposal_bytes) in batch_msg.mls_proposals.iter().enumerate() {
        match mls.decrypt(&group_name, proposal_bytes)? {
            DecryptResult::ProposalStored => {}
            other => {
                tracing::warn!(
                    "Violation: broken MLS proposal for group {} — proposal {} \
                     returned {:?}, expected ProposalStored",
                    group_name,
                    i,
                    other,
                );
                // BROKEN_MLS_PROPOSAL: IDs and digest match but MLS proposal is invalid.
                return Ok(ProcessResult::ViolationDetected(
                    ViolationEvidence::broken_mls_proposal(
                        steward_id.clone(),
                        handle.current_epoch(),
                        format!("proposal {i} returned {other:?}, expected ProposalStored"),
                    ),
                ));
            }
        }
    }

    // ── 5. Process commit ──────────────────────────────────────
    let res = mls.decrypt(&group_name, &batch_msg.commit_message)?;

    // ── 6. Clear only on successful commit outcomes ────────────
    match res {
        DecryptResult::CommitProcessed => {
            handle.clear_approved_proposals();
            Ok(ProcessResult::GroupUpdated)
        }
        DecryptResult::Removed => {
            handle.clear_approved_proposals();
            Ok(ProcessResult::LeaveGroup)
        }
        DecryptResult::Application(app_bytes) => {
            handle.clear_approved_proposals();
            AppMessage::decode(app_bytes.as_ref())?.try_into()
        }
        other => {
            tracing::warn!(
                "Unexpected commit result for group {}: {:?}, \
                 keeping approved proposals",
                group_name,
                other,
            );
            Ok(ProcessResult::Noop)
        }
    }
}

// ─────────────────────────── Proposal Queries ───────────────────────────

/// Get the count of approved proposals waiting to be committed.
///
/// Non-zero count indicates the steward should call [`create_batch_proposals`].
pub fn approved_proposals_count(handle: &GroupHandle) -> usize {
    handle.approved_proposals_count()
}

/// Get all approved proposals waiting to be committed.
///
/// Returns a map of proposal ID → [`GroupUpdateRequest`].
pub fn approved_proposals(handle: &GroupHandle) -> HashMap<ProposalId, GroupUpdateRequest> {
    handle.approved_proposals()
}

/// Get the epoch history (past batches of approved proposals).
///
/// Returns up to 10 most recent epochs, most recent last.
/// Useful for UI display of membership change history.
pub fn epoch_history(handle: &GroupHandle) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
    handle.epoch_history()
}

// ─────────────────────────── Steward Operations ───────────────────────────

/// Create and send batch proposals for the current epoch.
///
/// This is the steward's main responsibility: taking all approved proposals
/// and creating an MLS commit that applies them atomically.
///
/// # Arguments
/// * `handle` - The group handle (must be steward with approved proposals)
/// * `mls` - The MLS service for creating proposals and commit
///
/// # Returns
/// A vector of [`OutboundPacket`]s to send:
/// - Always includes the batch proposals message (proposals + commit)
/// - Includes a welcome message if new members are being added
///
/// # Errors
/// - [`CoreError::StewardNotSet`] if not the steward
/// - [`CoreError::NoProposals`] if no approved proposals
/// - [`CoreError::MlsGroupNotInitialized`] if MLS state is not initialized
/// - [`CoreError::MlsServiceError`] if MLS operations fail
///
/// # Side Effects
/// Clears approved proposals and archives them to epoch history.
pub fn create_batch_proposals<S>(
    handle: &mut GroupHandle,
    mls: &MlsService<S>,
) -> Result<Vec<OutboundPacket>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_steward() {
        return Err(CoreError::StewardNotSet);
    }

    let proposals = handle.approved_proposals();
    if proposals.is_empty() {
        return Err(CoreError::NoProposals);
    }

    if !handle.is_mls_initialized() {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    // Emergency criteria proposals are consensus-only — they don't produce MLS operations
    // and must NOT be in the approved queue at batch creation time.
    // handle_consensus_event is responsible for removing them immediately after approval.
    // If any are found here, it indicates a bug in the upstream flow.
    let emergency_ids: Vec<u32> = proposals
        .iter()
        .filter(|(_, req)| {
            matches!(
                req.payload,
                Some(group_update_request::Payload::EmergencyCriteria(_))
            )
        })
        .map(|(&id, _)| id)
        .collect();

    if !emergency_ids.is_empty() {
        return Err(CoreError::UnexpectedEmergencyProposals {
            proposal_ids: emergency_ids,
        });
    }

    let proposal_ids: Vec<u32> = proposals.keys().copied().collect();
    let proposals_digest = compute_proposals_digest(&proposals);
    let mut updates = Vec::with_capacity(proposals.len());
    for (_, proposal) in proposals {
        match proposal.payload {
            Some(group_update_request::Payload::InviteMember(im)) => {
                updates.push(GroupUpdate::Add(KeyPackageBytes::new(
                    im.key_package_bytes,
                    im.identity,
                )));
            }
            Some(group_update_request::Payload::RemoveMember(identity)) => {
                updates.push(GroupUpdate::Remove(identity.identity));
            }
            Some(group_update_request::Payload::EmergencyCriteria(_)) => {
                // Unreachable: emergency proposals are rejected above.
                return Err(CoreError::InvalidGroupUpdateRequest);
            }
            None => return Err(CoreError::InvalidGroupUpdateRequest),
        }
    }

    let CommitResult {
        proposals: mls_proposals,
        commit,
        welcome,
    } = mls.commit(handle.group_name(), &updates)?;

    // Create batch proposals message
    let batch_msg: AppMessage = BatchProposalsMessage {
        group_name: handle.group_name_bytes().to_vec(),
        mls_proposals,
        commit_message: commit,
        proposal_ids,
        proposals_digest,
    }
    .into();

    let batch_packet = OutboundPacket::new(
        batch_msg.encode_to_vec(),
        APP_MSG_SUBTOPIC,
        handle.group_name(),
        handle.app_id(),
    );

    let mut messages = vec![batch_packet];

    // Create welcome message if there are new members
    if let Some(welcome_bytes) = welcome {
        let welcome_msg: WelcomeMessage = invitation_from_bytes(welcome_bytes);
        let welcome_packet = OutboundPacket::new(
            welcome_msg.encode_to_vec(),
            WELCOME_SUBTOPIC,
            handle.group_name(),
            handle.app_id(),
        );
        messages.push(welcome_packet);
    }

    handle.clear_approved_proposals();

    Ok(messages)
}

// ─────────────────────────── Member Queries ───────────────────────────

/// Get the current members of a group.
///
/// Returns the identity bytes for each member.
///
/// # Errors
/// - [`CoreError::MlsGroupNotInitialized`] if MLS state is not initialized
/// - [`CoreError::MlsServiceError`] if member retrieval fails
pub fn group_members<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
) -> Result<Vec<Vec<u8>>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_mls_initialized() {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    let members = mls.members(handle.group_name())?;
    Ok(members)
}
