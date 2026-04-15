//! Inbound message routing and processing.

use super::freeze::process_commit_candidate;
use super::*;

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
            if group.is_steward() {
                info!(
                    "Steward received key package for group {}",
                    group.group_name()
                );
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;

                let gur = GroupUpdateRequest {
                    payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                        key_package_bytes,
                        identity,
                    })),
                };

                return Ok(ProcessResult::MembershipChangeReceived(gur));
            }
            Ok(ProcessResult::Noop)
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
        DecryptResult::Application(app_bytes, _sender) => {
            AppMessage::decode(app_bytes.as_ref())?.try_into()
        }
        DecryptResult::Removed(_) => Ok(ProcessResult::LeaveGroup),
        _ => {
            warn!("Unexpected MLS message type on app subtopic, ignoring");
            Ok(ProcessResult::Noop)
        }
    }
}
