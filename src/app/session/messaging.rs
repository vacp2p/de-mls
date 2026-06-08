//! Send operations on `SessionRunner`: key packages, app messages, and
//! ban requests.

use std::sync::Arc;

use prost::Message;

use crate::{
    app::{
        ConversationState, CreatorVote, SessionRunner, SessionTick, UserError,
        session::runner::send_packet,
    },
    core::{ConsensusPlugin, ConversationPluginsFactory},
    ds::{OutboundPacket, WELCOME_SUBTOPIC},
    mls_crypto::{KeyPackageBytes, MlsService},
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, ConversationMessage, ConversationUpdateRequest, MemberInvite,
        RemoveMember, conversation_update_request,
    },
};

/// Build a KP-broadcast packet for the welcome subtopic. The joiner
/// sends this so existing members can pick up the key package and
/// propose them for an Add.
pub fn build_key_package_packet(
    conversation_id: &str,
    key_package: KeyPackageBytes,
    app_id: &[u8],
) -> OutboundPacket {
    let invite = MemberInvite {
        key_package_bytes: key_package.as_bytes().to_vec(),
        member_id: key_package.member_id().to_vec(),
    };
    OutboundPacket::new(
        invite.encode_to_vec(),
        WELCOME_SUBTOPIC,
        conversation_id,
        app_id,
    )
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Broadcast `key_package` on this conversation's welcome subtopic so
    /// the steward can invite us.
    pub fn send_key_package(&self, key_package: KeyPackageBytes) -> Result<SessionTick, UserError> {
        let packet = build_key_package_packet(&self.conversation_id, key_package, &self.app_id);
        send_packet(self.transport(), packet)?;
        Ok(self.tick())
    }

    /// Send a chat message. Blocked in `PendingJoin` (no keys yet),
    /// `Freezing`, and `Selection` (epoch rotation in flight — the message
    /// might not decrypt on peers who have already merged the next commit).
    /// Governance traffic has its own gate (`check_proposal_allowed`).
    pub fn send_app_message(&mut self, message: Vec<u8>) -> Result<SessionTick, UserError> {
        let state = self.conversation.current_state();
        if matches!(
            state,
            ConversationState::PendingJoin
                | ConversationState::Freezing
                | ConversationState::Selection
        ) {
            return Err(UserError::ConversationBlocked(state.to_string()));
        }

        let app_msg: AppMessage = ConversationMessage {
            message,
            sender: self.member_id_display.to_string(),
            conversation_id: self.conversation_id.clone(),
        }
        .into();
        let app_id = Arc::clone(&self.app_id);
        let packet = self
            .conversation
            .expect_mls_mut()?
            .build_message(&app_msg, &app_id)?;
        send_packet(self.transport(), packet)?;
        Ok(self.tick())
    }

    /// Start a `RemoveMember` consensus round targeting `ban_request.user_to_ban`.
    /// The requester's click means "I want this person removed" → the
    /// creator's vote is bundled as YES at submit; no banner is shown to
    /// the requester.
    pub fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
    ) -> Result<SessionTick, UserError> {
        let state = self.conversation.current_state();
        if state != ConversationState::Working {
            return Err(UserError::ConversationBlocked(state.to_string()));
        }

        self.initiate_proposal(
            ConversationUpdateRequest {
                payload: Some(conversation_update_request::Payload::RemoveMember(
                    RemoveMember {
                        member_id: ban_request.user_to_ban,
                    },
                )),
            },
            CreatorVote::Yes,
        )?;

        Ok(self.tick())
    }
}
