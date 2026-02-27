use super::freeze::process_commit_candidate;
use super::*;

/// Process an inbound packet and determine what action is needed.
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
            if handle.is_steward() || handle.is_mls_initialized() {
                return Ok(ProcessResult::Noop);
            }

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

    // 1. Try plaintext CommitCandidate (sent as plaintext AppMessage)
    if let Ok(app_message) = AppMessage::decode(payload) {
        if let Some(app_message::Payload::CommitCandidate(candidate)) = app_message.payload {
            return process_commit_candidate(handle, candidate, mls);
        }
    }

    // 2. MLS-encrypted app messages only — use decrypt_application_only.
    //    This NEVER stores proposals or processes commits, preventing
    //    rogue MLS proposals on the app subtopic from polluting state.
    let res = mls.decrypt_application_only(handle.group_name(), payload)?;

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
