//! Inbound message routing and processing.

use openmls_rust_crypto::MemoryStorage;
use prost::Message;
use tracing::{info, warn};

use crate::core::api::process_commit_candidate;
use crate::core::{error::CoreError, group::Group, process_result::ProcessResult};
use crate::ds::{APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use crate::mls_crypto::{
    DeMlsStorage, DecryptResult, MlsService, key_package_bytes_from_json, parse_wallet_to_bytes,
};
use crate::protos::de_mls::messages::v1::{
    AppMessage, GroupUpdateRequest, InviteMember, WelcomeMessage, app_message,
    group_update_request, welcome_message,
};

/// Process an inbound packet and determine what action is needed.
pub fn process_inbound<S>(
    group: &mut Group,
    payload: &[u8],
    subtopic: &str,
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    match subtopic {
        WELCOME_SUBTOPIC => process_welcome_subtopic(group, payload, mls),
        APP_MSG_SUBTOPIC => process_app_subtopic(group, payload, mls),
        _ => Err(CoreError::InvalidSubtopic(subtopic.to_string())),
    }
}

fn process_welcome_subtopic<S>(
    group: &mut Group,
    payload: &[u8],
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let welcome_msg = WelcomeMessage::decode(payload)?;
    match welcome_msg.payload {
        Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
            let (key_package_bytes, identity) =
                key_package_bytes_from_json(user_kp.key_package_bytes)?;

            if mls.is_member(group.group_name(), &identity) {
                info!(
                    "Skipping KP for group {}: identity {:?} is already a member",
                    group.group_name(),
                    identity
                );
                return Ok(ProcessResult::Noop);
            }

            info!(
                "Received key package for group {} from identity {:?}",
                group.group_name(),
                identity
            );

            let gur = GroupUpdateRequest {
                payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                    key_package_bytes,
                    identity,
                })),
            };

            Ok(ProcessResult::MembershipChangeReceived(gur))
        }
        Some(welcome_message::Payload::InvitationToJoin(invitation)) => {
            if group.is_steward() || mls.has_group(group.group_name()) {
                return Ok(ProcessResult::Noop);
            }

            if mls.is_welcome_for_us(&invitation.mls_message_out_bytes)? {
                let group_name = mls.join_group(&invitation.mls_message_out_bytes)?;
                info!(
                    "[process_welcome_subtopic]: Joined group {}",
                    group.group_name()
                );
                return Ok(ProcessResult::JoinedGroup(group_name));
            }
            Ok(ProcessResult::Noop)
        }
        None => Ok(ProcessResult::Noop),
    }
}

fn process_app_subtopic<S>(
    group: &mut Group,
    payload: &[u8],
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !mls.has_group(group.group_name()) {
        return Ok(ProcessResult::Noop);
    }

    // 1. Try plaintext CommitCandidate (sent as plaintext AppMessage)
    if let Ok(app_message) = AppMessage::decode(payload) {
        if let Some(app_message::Payload::CommitCandidate(candidate)) = app_message.payload {
            return process_commit_candidate(group, candidate, mls);
        }
    }

    // 2. MLS-encrypted app messages only — use decrypt_application_only.
    //    This NEVER stores proposals or processes commits, preventing
    //    rogue MLS proposals on the app subtopic from polluting state.
    let res = mls.decrypt_application_only(group.group_name(), payload)?;

    match res {
        DecryptResult::Application(app_bytes, sender) => {
            let app_msg = AppMessage::decode(app_bytes.as_ref())?;
            // LeaveRequest is auto-approved — the MLS-authenticated sender
            // MUST equal the identity being removed. Reject mismatches here
            // so the app layer can trust the pair downstream.
            if let Some(app_message::Payload::LeaveRequest(leave)) = &app_msg.payload {
                if leave.identity != sender {
                    warn!(
                        "[process_app_subtopic] LeaveRequest signer mismatch for \
                         group {}: claimed={:?} sender={:?}",
                        group.group_name(),
                        leave.identity,
                        sender,
                    );
                    return Ok(ProcessResult::Noop);
                }
                return Ok(ProcessResult::LeaveRequestReceived(leave.clone()));
            }
            // Drop BanRequests whose target isn't in the group — saves a
            // useless consensus round. Mirrors the already-a-member check
            // on KPs in `process_welcome_subtopic`.
            if let Some(app_message::Payload::BanRequest(ban)) = &app_msg.payload {
                let target = parse_wallet_to_bytes(&ban.user_to_ban)?;
                if !mls.is_member(group.group_name(), &target) {
                    info!(
                        "Skipping BanRequest for group {}: target {:?} is not a member",
                        group.group_name(),
                        target
                    );
                    return Ok(ProcessResult::Noop);
                }
            }
            app_msg.try_into()
        }
        DecryptResult::Removed(_) => Ok(ProcessResult::LeaveGroup),
        DecryptResult::Ignored => {
            tracing::debug!("Ignored message on app subtopic (wrong epoch or group)");
            Ok(ProcessResult::Noop)
        }
        _ => {
            warn!("Unexpected MLS message type on app subtopic, ignoring");
            Ok(ProcessResult::Noop)
        }
    }
}
