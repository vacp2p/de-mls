//! Wire framing helpers for welcome-subtopic packets. Conversation
//! construction lives on
//! [`Conversation::new`](crate::core::Conversation::new); application-message
//! framing lives on
//! [`MlsService::build_message`](crate::mls_crypto::MlsService::build_message);
//! consensus `CreateProposalRequest` construction lives in the
//! `crate::app::session` consensus-bridge helpers.

use prost::Message;

use crate::{
    ds::{OutboundPacket, WELCOME_SUBTOPIC},
    mls_crypto::KeyPackageBytes,
    protos::de_mls::messages::v1::{InvitationToJoin, UserKeyPackage, WelcomeMessage},
};

// ─────────────────────────── Welcome-subtopic framing ───────────────────────────

/// Build a key package message for joining a conversation.
///
/// The key package is supplied by the caller (typically via
/// `OpenMlsService::generate_key_package`) so a joiner can publish a key
/// package before any MLS service for the target conversation exists.
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
