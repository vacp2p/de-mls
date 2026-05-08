//! Wire framing helpers: welcome-subtopic packets and consensus
//! `CreateProposalRequest` construction. Conversation construction lives on
//! [`Conversation::create`](crate::core::Conversation::create) and
//! [`Conversation::prepare_to_join`](crate::core::Conversation::prepare_to_join);
//! application-message framing lives on
//! [`MlsService::build_message`](crate::mls_crypto::MlsService::build_message).

use hashgraph_like_consensus::types::CreateProposalRequest;
use prost::Message;

use crate::{
    core::error::CoreError,
    ds::{OutboundPacket, WELCOME_SUBTOPIC},
    mls_crypto::KeyPackageBytes,
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, InvitationToJoin, UserKeyPackage, WelcomeMessage,
    },
};

// ─────────────────────────── Welcome-subtopic framing ───────────────────────────

/// Build a key package message for joining a group.
///
/// The key package is supplied by the caller (typically via
/// `OpenMlsService::generate_key_package`) so a joiner can publish a key
/// package before any MLS service for the target group exists.
pub fn build_key_package_message(
    conversation_name: &str,
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
        conversation_name,
        app_id,
    )
}

/// Wrap raw MLS welcome bytes into an `OutboundPacket` on the welcome subtopic.
pub(crate) fn build_invitation_packet(
    welcome_bytes: Vec<u8>,
    conversation_name: &str,
    app_id: &[u8],
) -> OutboundPacket {
    let welcome_msg: WelcomeMessage = InvitationToJoin {
        mls_message_out_bytes: welcome_bytes,
    }
    .into();
    OutboundPacket::new(
        welcome_msg.encode_to_vec(),
        WELCOME_SUBTOPIC,
        conversation_name,
        app_id,
    )
}

// ─────────────────────────── Consensus Proposal Building ───────────────────────────

/// Build the consensus-library `CreateProposalRequest` for a
/// `ConversationUpdateRequest`. Pure — no async, no I/O. The caller (app
/// layer) submits the resulting request via
/// `ProviderConsensus::create_proposal_with_config`.
pub fn build_create_proposal_request(
    request: &ConversationUpdateRequest,
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
