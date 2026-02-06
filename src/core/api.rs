//! Core API for MLS group operations.
//!
//! This module provides the fundamental building blocks for MLS group management.
//! All functions are free-standing and operate on [`GroupHandle`] instances.
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
//!   create_group() → GroupHandle (steward=true, MLS initialized)
//!
//! Member joins:
//!   prepare_to_join() → GroupHandle (steward=false, MLS=None)
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

use prost::Message;
use std::collections::{HashMap, VecDeque};
use tracing::info;

use crate::core::{
    error::CoreError, group_handle::GroupHandle, types::invitation_from_bytes,
    types::ProcessResult, ProposalId,
};
use crate::ds::{OutboundPacket, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use crate::mls_crypto::{
    key_package_bytes_from_json, BatchProposalsResult, IdentityService, KeyPackageBytes,
    MlsGroupService, MlsGroupUpdate, MlsProcessResult,
};
use crate::protos::de_mls::messages::v1::{
    app_message, group_update_request, welcome_message, AppMessage, BatchProposalsMessage,
    GroupUpdateRequest, InviteMember, UserKeyPackage, WelcomeMessage,
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
/// A [`GroupHandle`] with MLS state initialized and steward=true.
///
/// # Errors
/// Returns [`CoreError::MlsError`] if group creation fails.
pub fn create_group(name: &str, mls: &dyn MlsGroupService) -> Result<GroupHandle, CoreError> {
    let mls_handle = mls.create_group(name)?;
    Ok(GroupHandle::new_as_creator(name, mls_handle))
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
/// A [`GroupHandle`] ready for joining (steward=false, MLS=None).
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
/// Returns [`CoreError::MlsError`] if welcome processing fails.
pub fn join_group_from_invite(
    handle: &mut GroupHandle,
    welcome_bytes: &[u8],
    mls: &dyn MlsGroupService,
) -> Result<String, CoreError> {
    let (mls_handle, group_id) = mls.join_group_from_invite(welcome_bytes)?;
    let group_name =
        String::from_utf8(group_id).unwrap_or_else(|_| handle.group_name().to_string());
    handle.set_mls_handle(mls_handle);
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
/// * `handle` - The group handle (must have MLS initialized)
/// * `mls` - The MLS service for encryption
/// * `app_msg` - The application message to encrypt
///
/// # Returns
/// An [`OutboundPacket`] containing the encrypted message.
///
/// # Errors
/// - [`CoreError::MlsGroupNotInitialized`] if MLS state is not initialized
/// - [`CoreError::MlsError`] if encryption fails
pub async fn build_message(
    handle: &GroupHandle,
    mls: &dyn MlsGroupService,
    app_msg: &AppMessage,
) -> Result<OutboundPacket, CoreError> {
    let mls_handle = handle
        .mls_handle()
        .ok_or(CoreError::MlsGroupNotInitialized)?;

    let message_out = {
        let mut mls_group = mls_handle.lock().await;
        mls.build_message(&mut mls_group, &app_msg.encode_to_vec())?
    };

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
/// * `identity` - The identity service for key package generation
///
/// # Returns
/// An [`OutboundPacket`] containing the key package on the welcome subtopic.
///
/// # Errors
/// Returns [`CoreError::IdentityError`] if key package generation fails.
pub fn build_key_package_message<I: IdentityService>(
    handle: &GroupHandle,
    identity: &mut I,
) -> Result<OutboundPacket, CoreError> {
    let key_package = identity.generate_key_package()?;
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
/// * `mls` - The MLS service for decryption
/// * `identity` - The identity service for key package verification
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
/// - [`CoreError::MlsError`] if decryption fails
/// - [`CoreError::MessageError`] if message parsing fails
///
/// # Next Steps
/// Pass the result to [`dispatch_result`](super::dispatch_result) to handle
/// consensus routing and get a [`DispatchAction`](super::DispatchAction).
pub async fn process_inbound<M, I>(
    handle: &mut GroupHandle,
    payload: &[u8],
    subtopic: &str,
    mls: &M,
    identity: &I,
) -> Result<ProcessResult, CoreError>
where
    M: MlsGroupService,
    I: IdentityService,
{
    match subtopic {
        WELCOME_SUBTOPIC => process_welcome_subtopic(handle, payload, mls, identity).await,
        APP_MSG_SUBTOPIC => process_app_subtopic(handle, payload, mls).await,
        _ => Err(CoreError::InvalidSubtopic(subtopic.to_string())),
    }
}

async fn process_welcome_subtopic<M, I>(
    handle: &mut GroupHandle,
    payload: &[u8],
    mls: &M,
    identity: &I,
) -> Result<ProcessResult, CoreError>
where
    M: MlsGroupService,
    I: IdentityService,
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
            let hash_refs =
                mls.invite_new_member_hash_refs(invitation.mls_message_out_bytes.as_slice())?;
            if hash_refs
                .iter()
                .any(|hash_ref| identity.is_key_package_exists(hash_ref))
            {
                let (mls_handle, _) =
                    mls.join_group_from_invite(invitation.mls_message_out_bytes.as_slice())?;
                handle.set_mls_handle(mls_handle);
                info!(
                    "[process_welcome_subtopic]: Joined group {}",
                    handle.group_name()
                );
                return Ok(ProcessResult::JoinedGroup(handle.group_name().to_string()));
            }
            Ok(ProcessResult::Noop)
        }
        None => Ok(ProcessResult::Noop),
    }
}

async fn process_app_subtopic(
    handle: &mut GroupHandle,
    payload: &[u8],
    mls: &dyn MlsGroupService,
) -> Result<ProcessResult, CoreError> {
    if !handle.is_mls_initialized() {
        return Ok(ProcessResult::Noop);
    }

    // Try to parse as AppMessage first (for batch proposals)
    if let Ok(app_message) = AppMessage::decode(payload) {
        if let Some(app_message::Payload::BatchProposalsMessage(batch_msg)) = app_message.payload {
            return process_batch_proposals(handle, batch_msg, mls).await;
        }
    }

    // Fall back to MLS protocol message
    let mls_handle = handle
        .mls_handle()
        .ok_or(CoreError::MlsGroupNotInitialized)?;

    let res = {
        let mut mls_group = mls_handle.lock().await;
        mls.process_inbound(&mut mls_group, payload)?
    };

    match res {
        MlsProcessResult::Application(app_bytes) => {
            AppMessage::decode(app_bytes.as_ref())?.try_into()
        }
        MlsProcessResult::LeaveGroup => Ok(ProcessResult::LeaveGroup),
        MlsProcessResult::Noop => Ok(ProcessResult::Noop),
    }
}

async fn process_batch_proposals(
    handle: &mut GroupHandle,
    batch_msg: BatchProposalsMessage,
    mls: &dyn MlsGroupService,
) -> Result<ProcessResult, CoreError> {
    // Verify that we have approved proposals before applying the batch.
    // Members should only accept batches that match their consensus-approved list.
    if handle.approved_proposals_count() == 0 {
        tracing::warn!(
            "Rejecting batch proposals for group {}: no approved proposals to match",
            handle.group_name()
        );
        return Ok(ProcessResult::Noop);
    }

    let mls_handle = handle
        .mls_handle()
        .ok_or(CoreError::MlsGroupNotInitialized)?;

    // Process all proposals
    for proposal_bytes in &batch_msg.mls_proposals {
        let mut mls_group = mls_handle.lock().await;
        let _ = mls.process_inbound(&mut mls_group, proposal_bytes)?;
    }

    // Process the commit
    let res = {
        let mut mls_group = mls_handle.lock().await;
        mls.process_inbound(&mut mls_group, &batch_msg.commit_message)?
    };

    // Clear approved proposals after successful batch application (archives to history)
    handle.clear_approved_proposals();

    match res {
        MlsProcessResult::Application(app_bytes) => {
            AppMessage::decode(app_bytes.as_ref())?.try_into()
        }
        MlsProcessResult::LeaveGroup => Ok(ProcessResult::LeaveGroup),
        MlsProcessResult::Noop => Ok(ProcessResult::GroupUpdated),
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
/// - [`CoreError::MlsError`] if MLS operations fail
///
/// # Side Effects
/// Clears approved proposals and archives them to epoch history.
pub async fn create_batch_proposals(
    handle: &mut GroupHandle,
    mls: &dyn MlsGroupService,
) -> Result<Vec<OutboundPacket>, CoreError> {
    if !handle.is_steward() {
        return Err(CoreError::StewardNotSet);
    }

    let proposals = handle.approved_proposals();
    if proposals.is_empty() {
        return Err(CoreError::NoProposals);
    }
    handle.clear_approved_proposals();

    let mls_handle = handle
        .mls_handle()
        .ok_or(CoreError::MlsGroupNotInitialized)?;

    let mut updates = Vec::with_capacity(proposals.len());
    for (_, proposal) in proposals {
        match proposal.payload {
            Some(group_update_request::Payload::InviteMember(im)) => {
                updates.push(MlsGroupUpdate::AddMember(KeyPackageBytes::new(
                    im.key_package_bytes,
                    im.identity,
                )));
            }
            Some(group_update_request::Payload::RemoveMember(identity)) => {
                updates.push(MlsGroupUpdate::RemoveMember(identity.identity));
            }
            None => return Err(CoreError::InvalidGroupUpdateRequest),
        }
    }

    let BatchProposalsResult {
        proposals: mls_proposals,
        commit,
        welcome,
    } = {
        let mut mls_group = mls_handle.lock().await;
        mls.create_batch_proposals(&mut mls_group, &updates)?
    };

    // Create batch proposals message
    let batch_msg: AppMessage = BatchProposalsMessage {
        group_name: handle.group_name_bytes().to_vec(),
        mls_proposals,
        commit_message: commit,
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

    Ok(messages)
}

// ─────────────────────────── Member Queries ───────────────────────────

/// Get the current members of a group.
///
/// Returns the identity bytes for each member.
///
/// # Errors
/// - [`CoreError::MlsGroupNotInitialized`] if MLS state is not initialized
/// - [`CoreError::MlsError`] if member retrieval fails
pub async fn group_members(
    handle: &GroupHandle,
    mls: &dyn MlsGroupService,
) -> Result<Vec<Vec<u8>>, CoreError> {
    let mls_handle = handle
        .mls_handle()
        .ok_or(CoreError::MlsGroupNotInitialized)?;

    let mls_group = mls_handle.lock().await;
    let members = mls.group_members(&mls_group)?;
    Ok(members)
}
