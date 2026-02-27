use super::*;

// ─────────────────────────── Group Lifecycle ───────────────────────────

/// Create a new MLS group as the steward.
pub fn create_group<S>(name: &str, mls: &MlsService<S>) -> Result<GroupHandle, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    mls.create_group(name)?;
    Ok(GroupHandle::new_as_creator(name, mls.wallet_bytes()))
}

/// Prepare a handle for joining an existing group.
pub fn prepare_to_join(name: &str) -> GroupHandle {
    GroupHandle::new_for_join(name)
}

/// Complete joining a group using a welcome message.
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

// ─────────────────────────── Message Building ───────────────────────────

/// Build an MLS-encrypted application message.
pub fn build_message<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
    app_msg: &AppMessage,
    app_id: &[u8],
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
        app_id,
    ))
}

/// Build a key package message for joining a group.
pub fn build_key_package_message<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
    app_id: &[u8],
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
        app_id,
    ))
}

// ─────────────────────────── Inbound Processing ───────────────────────────
