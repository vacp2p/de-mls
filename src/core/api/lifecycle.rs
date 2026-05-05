//! Group lifecycle operations: creation, joining, and message building.

use hashgraph_like_consensus::types::CreateProposalRequest;
use prost::Message;

use crate::{
    core::{error::CoreError, group::Group, steward_list::ProtocolConfig},
    ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC},
    mls_crypto::{KeyPackageBytes, MlsService},
    protos::de_mls::messages::v1::{
        AppMessage, GroupUpdateRequest, InvitationToJoin, UserKeyPackage, WelcomeMessage,
    },
};

// ─────────────────────────── Group Lifecycle ───────────────────────────

/// Create a new MLS group as the steward.
///
/// Initializes a bootstrap steward list containing the creator. The MLS
/// group itself is constructed by the caller (typically via
/// `OpenMlsService::new_as_creator`); this function builds only the
/// app-level [`Group`] wrapping it.
pub fn create_group(
    name: &str,
    self_identity: Vec<u8>,
    protocol_config: ProtocolConfig,
) -> Result<Group, CoreError> {
    Group::new_as_creator(name, self_identity, protocol_config)
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

/// Build an MLS-encrypted application message for `mls`'s group.
pub fn build_message<M>(
    mls: &M,
    app_msg: &AppMessage,
    app_id: &[u8],
) -> Result<OutboundPacket, CoreError>
where
    M: MlsService,
{
    let message_out = mls.encrypt(&app_msg.encode_to_vec())?;

    Ok(OutboundPacket::new(
        message_out,
        APP_MSG_SUBTOPIC,
        mls.group_id(),
        app_id,
    ))
}

/// Build a key package message for joining a group.
///
/// The key package is supplied by the caller (typically via
/// `OpenMlsService::generate_key_package`) so a joiner can publish a key
/// package before any MLS service for the target group exists.
pub fn build_key_package_message(
    group: &Group,
    key_package: KeyPackageBytes,
    app_id: &[u8],
) -> OutboundPacket {
    let welcome_msg: WelcomeMessage = UserKeyPackage {
        key_package_bytes: key_package.as_bytes().to_vec(),
    }
    .into();

    OutboundPacket::new(
        welcome_msg.encode_to_vec(),
        WELCOME_SUBTOPIC,
        group.group_name(),
        app_id,
    )
}

/// Wrap raw MLS welcome bytes into an `OutboundPacket` on the welcome subtopic.
pub(crate) fn build_invitation_packet(
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

// ─────────────────────────── Consensus Proposal Building ───────────────────────────

/// Build the consensus-library `CreateProposalRequest` for a
/// `GroupUpdateRequest`. Pure — no async, no I/O. The caller (app
/// layer) submits the resulting request via
/// `ProviderConsensus::create_proposal_with_config`.
pub fn build_create_proposal_request(
    request: &GroupUpdateRequest,
    creator_id: &[u8],
    expected_voters: u32,
    proposal_expiration_secs: u64,
    liveness_criteria_yes: bool,
) -> Result<CreateProposalRequest, CoreError> {
    let payload = request.encode_to_vec();
    Ok(CreateProposalRequest::new(
        uuid::Uuid::new_v4().to_string(),
        payload,
        creator_id.to_vec(),
        expected_voters,
        proposal_expiration_secs,
        liveness_criteria_yes,
    )?)
}
