//! Group lifecycle operations: creation, joining, and message building.

use openmls_rust_crypto::MemoryStorage;
use prost::Message;

use crate::{
    core::{error::CoreError, group::Group, steward_list::ProtocolConfig},
    ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC},
    mls_crypto::{DeMlsStorage, MlsService},
    protos::de_mls::messages::v1::{AppMessage, InvitationToJoin, UserKeyPackage, WelcomeMessage},
};

// ─────────────────────────── Group Lifecycle ───────────────────────────

/// Create a new MLS group as the steward.
///
/// Initializes a bootstrap steward list containing the creator.
pub fn create_group<S>(
    name: &str,
    mls: &MlsService<S>,
    protocol_config: ProtocolConfig,
) -> Result<Group, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    mls.create_group(name)?;
    Group::new_as_creator(name, mls.wallet_bytes().to_vec(), protocol_config)
}

/// Prepare a handle for joining an existing group.
///
/// The joiner knows the config from group discovery but won't have the
/// actual steward list until receiving it via sync message.
pub fn prepare_to_join(
    name: &str,
    self_identity: Vec<u8>,
    protocol_config: ProtocolConfig,
) -> Group {
    Group::new_as_joiner(name, self_identity, protocol_config)
}

// ─────────────────────────── Message Building ───────────────────────────

/// Build an MLS-encrypted application message.
pub fn build_message<S>(
    group_name: &str,
    mls: &MlsService<S>,
    app_msg: &AppMessage,
    app_id: &[u8],
) -> Result<OutboundPacket, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !mls.has_group(group_name) {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    let message_out = mls.encrypt(group_name, &app_msg.encode_to_vec())?;

    Ok(OutboundPacket::new(
        message_out,
        APP_MSG_SUBTOPIC,
        group_name,
        app_id,
    ))
}

/// Build a key package message for joining a group.
pub fn build_key_package_message<S>(
    group: &Group,
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
        group.group_name(),
        app_id,
    ))
}

/// Wrap raw MLS welcome bytes into an `OutboundPacket` on the welcome subtopic.
pub fn build_invitation_packet(
    welcome_bytes: Vec<u8>,
    group: &Group,
    app_id: &[u8],
) -> OutboundPacket {
    let welcome_msg: WelcomeMessage = InvitationToJoin {
        mls_message_out_bytes: welcome_bytes,
    }
    .into();
    OutboundPacket::new(
        welcome_msg.encode_to_vec(),
        WELCOME_SUBTOPIC,
        group.group_name(),
        app_id,
    )
}
