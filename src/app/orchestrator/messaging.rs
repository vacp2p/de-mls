//! Send operations on `SessionRunner`: key packages and app messages.
//!
//! `process_ban_request` is the third per-conversation send operation but
//! depends on `initiate_proposal`, which still lives on `User` until the
//! consensus methods migrate. It stays on `User` for now and will move to
//! `SessionRunner` together with the consensus surface.

use crate::{
    app::{ConversationState, SessionRunner, User, UserError},
    core::{ConsensusPlugin, ConversationPluginsFactory, build_key_package_message},
    identity::parse_wallet_to_bytes,
    mls_crypto::{KeyPackageBytes, MlsService},
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, ConversationMessage, ConversationUpdateRequest, RemoveMember,
        conversation_update_request,
    },
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Broadcast `key_package` on this conversation's welcome subtopic so
    /// the steward can invite us. The caller (typically the integrator)
    /// generates the key package via [`User::generate_key_package`] — KP
    /// minting is identity-bound, not conversation-bound, so it stays at
    /// the User layer.
    pub async fn send_kp_message(&self, key_package: KeyPackageBytes) -> Result<(), UserError> {
        let packet = build_key_package_message(&self.conversation_name, key_package, &self.app_id);
        self.send_outbound(packet).await?;
        Ok(())
    }

    /// Send a chat message. Blocked in `PendingJoin` (no keys yet),
    /// `Freezing`, and `Selection` (epoch rotation in flight — the message
    /// might not decrypt on peers who have already merged the next commit).
    /// Governance traffic has its own gate (`check_proposal_allowed`).
    pub async fn send_app_message(&self, message: Vec<u8>) -> Result<(), UserError> {
        let state = self.handle.current_state();
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
            sender: self.identity_display.to_string(),
            conversation_name: self.conversation_name.clone(),
        }
        .into();

        let packet = self
            .handle
            .expect_mls()?
            .build_message(&app_msg, &self.app_id)?;
        self.send_outbound(packet).await?;
        Ok(())
    }
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
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
