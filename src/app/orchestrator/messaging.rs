//! Send operations (key packages, app messages, ban requests).

use crate::{
    app::{ConversationState, User, UserError},
    core::{ConsensusPlugin, ConversationPluginsFactory, build_key_package_message},
    identity::parse_wallet_to_bytes,
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, ConversationMessage, ConversationUpdateRequest, RemoveMember,
        conversation_update_request,
    },
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Broadcast our key-package on the welcome subtopic so the steward
    /// can invite us.
    pub async fn send_kp_message(&self, conversation_name: &str) -> Result<(), UserError> {
        if !self
            .conversations
            .read()
            .await
            .contains_key(conversation_name)
        {
            return Err(UserError::ConversationNotFound);
        }
        let key_package = self.generate_key_package()?;
        let packet = build_key_package_message(conversation_name, key_package, &self.app_id);
        self.handler.on_outbound(conversation_name, packet).await?;
        Ok(())
    }

    /// Send a chat message. Blocked in `PendingJoin` (no keys yet),
    /// `Freezing`, and `Selection` (epoch rotation in flight — the message
    /// might not decrypt on peers who have already merged the next commit).
    /// Governance traffic has its own gate (`check_proposal_allowed`).
    pub async fn send_app_message(
        &self,
        conversation_name: &str,
        message: Vec<u8>,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let packet = {
            let entry = entry_arc.read().await;
            let state = entry.handle.current_state();
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
                sender: self.identity_string(),
                conversation_name: conversation_name.to_string(),
            }
            .into();

            entry
                .handle
                .expect_mls()?
                .build_message(&app_msg, &self.app_id)?
        };
        self.handler.on_outbound(conversation_name, packet).await?;
        Ok(())
    }

    /// Start a `RemoveMember` consensus round targeting `ban_request.user_to_ban`.
    /// The requester's click means "I want this person removed" → the
    /// creator's vote is bundled as YES at submit; no banner is shown to
    /// the requester.
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        conversation_name: &str,
    ) -> Result<(), UserError> {
        {
            let entry_arc = self
                .lookup_entry(conversation_name)
                .await
                .ok_or(UserError::ConversationNotFound)?;
            let entry = entry_arc.read().await;
            let state = entry.handle.current_state();
            if state != ConversationState::Working {
                return Err(UserError::ConversationBlocked(state.to_string()));
            }
        }

        self.initiate_proposal(
            conversation_name.to_string(),
            ConversationUpdateRequest {
                payload: Some(conversation_update_request::Payload::RemoveMember(
                    RemoveMember {
                        identity: parse_wallet_to_bytes(ban_request.user_to_ban.as_str())?,
                    },
                )),
            },
            Some(true),
        )
        .await?;

        Ok(())
    }
}
