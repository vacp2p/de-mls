//! Free function API for group operations.
//!
//! This module provides the main API for working with MLS groups.
//! All functions operate on a `GroupHandle` and use traits for
//! MLS operations and event handling.

use alloy::hex;
use prost::Message;
use tracing::info;

use ds::{transport::OutboundPacket, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::{
    BatchProposalsResult, IdentityService, KeyPackageBytes, MlsGroupService, MlsGroupUpdate,
    MlsProcessResult,
};

use super::error::CoreError;
use super::group_handle::GroupHandle;
use super::types::{GroupUpdateRequest, ProcessResult};
use crate::core::types::invitation_from_bytes;
use crate::protos::de_mls::messages::v1::{
    app_message, welcome_message, AppMessage, BatchProposalsMessage, UpdateRequest, UserKeyPackage,
    WelcomeMessage,
};

// ─────────────────────────── Group Lifecycle ───────────────────────────

/// Create a new MLS group.
///
/// # Arguments
/// * `name` - The name of the group
/// * `mls` - The MLS service for group creation
///
/// # Returns
/// A new `GroupHandle` for the created group (as steward).
pub fn create_group(name: &str, mls: &dyn MlsGroupService) -> Result<GroupHandle, CoreError> {
    let mls_handle = mls.create_group(name)?;
    Ok(GroupHandle::new_as_creator(name, mls_handle))
}

/// Prepare to join a group (creates handle without MLS group).
///
/// # Arguments
/// * `name` - The name of the group to join
///
/// # Returns
/// A new `GroupHandle` ready for joining.
pub fn prepare_to_join(name: &str) -> GroupHandle {
    GroupHandle::new_for_join(name)
}

/// Join a group using a welcome message.
///
/// # Arguments
/// * `handle` - The group handle (must be prepared with `prepare_to_join`)
/// * `welcome_bytes` - The serialized MLS welcome message
/// * `mls` - The MLS service for processing the welcome
///
/// # Returns
/// The group name extracted from the welcome message.
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
/// # Arguments
/// * `handle` - The group handle
pub fn become_steward(handle: &mut GroupHandle) {
    handle.become_steward();
}

/// Resign as steward of a group.
///
/// # Arguments
/// * `handle` - The group handle
pub fn resign_steward(handle: &mut GroupHandle) {
    handle.resign_steward();
}

/// Check if the handle is the steward.
///
/// # Arguments
/// * `handle` - The group handle
pub fn is_steward(handle: &GroupHandle) -> bool {
    handle.is_steward()
}

// ─────────────────────────── Message Building ───────────────────────────

/// Build an MLS-encrypted application message for the group.
///
/// # Arguments
/// * `handle` - The group handle
/// * `mls` - The MLS service for message encryption
/// * `app_msg` - The application message to send
///
/// # Returns
/// An outbound packet ready to be sent via the delivery service.
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
/// # Arguments
/// * `handle` - The group handle (for metadata)
/// * `identity` - The identity service for key package generation
///
/// # Returns
/// An outbound packet containing the key package for the welcome topic.
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

/// Process an inbound packet and return the result.
///
/// # Arguments
/// * `handle` - The group handle
/// * `payload` - The raw packet payload
/// * `subtopic` - The subtopic the packet was received on
/// * `mls` - The MLS service for message processing
/// * `identity` - The identity service for key package operations
///
/// # Returns
/// A `ProcessResult` indicating what happened.
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
        _ => Ok(ProcessResult::Noop),
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
                    "[process_welcome_subtopic]: Steward received key package for group {}",
                    handle.group_name()
                );
                let key_package =
                    mls_crypto::key_package_bytes_from_json(user_kp.key_package_bytes)?;

                if let Some(request) = handle.store_add_proposal(key_package) {
                    return Ok(ProcessResult::MemberProposalAdded(request));
                }
            }
            Ok(ProcessResult::Noop)
        }
        Some(welcome_message::Payload::InvitationToJoin(invitation)) => {
            // Only non-stewards who haven't joined yet process invitations
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
            process_application_message(handle, &app_bytes).await
        }
        MlsProcessResult::LeaveGroup => Ok(ProcessResult::LeaveGroup),
        MlsProcessResult::Noop => Ok(ProcessResult::Noop),
    }
}

async fn process_application_message(
    handle: &mut GroupHandle,
    message_bytes: &[u8],
) -> Result<ProcessResult, CoreError> {
    let app_msg = AppMessage::decode(message_bytes)?;

    match &app_msg.payload {
        Some(app_message::Payload::ConversationMessage(_)) => {
            Ok(ProcessResult::AppMessage(app_msg))
        }
        Some(app_message::Payload::Proposal(proposal)) => {
            Ok(ProcessResult::Proposal(proposal.clone()))
        }
        Some(app_message::Payload::Vote(vote)) => Ok(ProcessResult::Vote(vote.clone())),
        Some(app_message::Payload::BanRequest(ban_request)) => {
            // If steward, auto-add remove proposal
            if handle.is_steward() {
                let remove_request = handle.store_remove_proposal(ban_request.user_to_ban.clone());
                Ok(ProcessResult::MemberProposalAdded(remove_request.unwrap()))
            } else {
                Ok(ProcessResult::Noop)
            }
        }
        _ => Ok(ProcessResult::Noop),
    }
}

async fn process_batch_proposals(
    handle: &mut GroupHandle,
    batch_msg: BatchProposalsMessage,
    mls: &dyn MlsGroupService,
) -> Result<ProcessResult, CoreError> {
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

    match res {
        MlsProcessResult::Application(app_bytes) => {
            process_application_message(handle, &app_bytes).await
        }
        MlsProcessResult::LeaveGroup => Ok(ProcessResult::LeaveGroup),
        MlsProcessResult::Noop => Ok(ProcessResult::Noop),
    }
}

// ─────────────────────────── Steward Operations ───────────────────────────

/// Store an add member proposal.
///
/// # Arguments
/// * `handle` - The group handle (must be steward)
/// * `key_package` - The key package of the member to add
pub fn store_add_proposal(
    handle: &mut GroupHandle,
    key_package: KeyPackageBytes,
) -> Result<UpdateRequest, CoreError> {
    handle
        .store_add_proposal(key_package)
        .ok_or(CoreError::StewardNotSet)
}

/// Store a remove member proposal.
///
/// # Arguments
/// * `handle` - The group handle (must be steward)
/// * `identity` - The wallet address of the member to remove
pub fn store_remove_proposal(
    handle: &mut GroupHandle,
    identity: String,
) -> Result<UpdateRequest, CoreError> {
    handle
        .store_remove_proposal(identity)
        .ok_or(CoreError::StewardNotSet)
}

/// Get the count of approved proposals.
pub fn approved_proposals_count(handle: &GroupHandle) -> usize {
    handle.approved_proposals_count()
}

/// Get the approved proposals.
pub fn approved_proposals(handle: &GroupHandle) -> Vec<GroupUpdateRequest> {
    handle.approved_proposals()
}

/// Start a voting epoch and return the number of proposals.
pub fn start_voting_epoch(handle: &mut GroupHandle) -> Result<usize, CoreError> {
    handle.start_voting_epoch().ok_or(CoreError::StewardNotSet)
}

/// Get the voting proposals.
pub fn voting_proposals(handle: &GroupHandle) -> Vec<GroupUpdateRequest> {
    handle.voting_proposals()
}

/// Create batch proposals message for the voting epoch.
///
/// # Arguments
/// * `handle` - The group handle (must be steward with voting proposals)
/// * `mls` - The MLS service for proposal creation
///
/// # Returns
/// A vector of outbound packets (batch proposals and optional welcome).
pub async fn create_batch_proposals(
    handle: &mut GroupHandle,
    mls: &dyn MlsGroupService,
) -> Result<Vec<OutboundPacket>, CoreError> {
    if !handle.is_steward() {
        return Err(CoreError::StewardNotSet);
    }

    let proposals = handle.voting_proposals();
    if proposals.is_empty() {
        return Err(CoreError::NoProposals);
    }

    let mls_handle = handle
        .mls_handle()
        .ok_or(CoreError::MlsGroupNotInitialized)?;

    let mut updates = Vec::with_capacity(proposals.len());
    for proposal in proposals {
        match proposal {
            GroupUpdateRequest::AddMember(key_package_bytes) => {
                updates.push(MlsGroupUpdate::AddMember(key_package_bytes));
            }
            GroupUpdateRequest::RemoveMember(identity) => {
                let identity_bytes = if let Some(hex_string) = identity.strip_prefix("0x") {
                    hex::decode(hex_string)?
                } else {
                    hex::decode(&identity)?
                };
                updates.push(MlsGroupUpdate::RemoveMember(identity_bytes));
            }
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

/// Complete voting and clear proposals.
pub fn complete_voting(handle: &mut GroupHandle, _accepted: bool) {
    handle.complete_voting();
}

// ─────────────────────────── Queries ───────────────────────────

/// Get the members of a group.
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

/// Get the current epoch of a group.
pub async fn group_epoch(
    handle: &GroupHandle,
    mls: &dyn MlsGroupService,
) -> Result<u64, CoreError> {
    let mls_handle = handle
        .mls_handle()
        .ok_or(CoreError::MlsGroupNotInitialized)?;

    let mls_group = mls_handle.lock().await;
    let epoch = mls.group_epoch(&mls_group)?;
    Ok(epoch)
}
