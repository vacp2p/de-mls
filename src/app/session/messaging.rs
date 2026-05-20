//! Send operations on `SessionRunner`: key packages, app messages, and
//! ban requests.

use std::sync::{Arc, RwLock};

use super::lock::LockExt;
use super::runner::send_packet;

use crate::{
    app::{ConversationState, CreatorVote, SessionRunner, UserError},
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
    /// generates the key package via [`crate::app::User::generate_key_package`] —
    /// KP minting is identity-bound, not conversation-bound, so it stays
    /// at the User layer.
    ///
    /// Takes `&Arc<RwLock<Self>>` so the runner lock is released before
    /// awaiting on the transport.
    pub async fn send_kp_message(
        arc: &Arc<RwLock<Self>>,
        key_package: KeyPackageBytes,
    ) -> Result<(), UserError> {
        let (transport, packet) = {
            let s = arc.read_or_err("session")?;
            let packet = build_key_package_message(&s.conversation_name, key_package, &s.app_id);
            (Arc::clone(s.transport()), packet)
        };
        send_packet(&transport, packet)?;
        Ok(())
    }

    /// Send a chat message. Blocked in `PendingJoin` (no keys yet),
    /// `Freezing`, and `Selection` (epoch rotation in flight — the message
    /// might not decrypt on peers who have already merged the next commit).
    /// Governance traffic has its own gate (`check_proposal_allowed`).
    ///
    /// Takes `&Arc<RwLock<Self>>` so the runner lock is released before
    /// awaiting on the transport.
    pub async fn send_app_message(
        arc: &Arc<RwLock<Self>>,
        message: Vec<u8>,
    ) -> Result<(), UserError> {
        let (transport, packet) = {
            let s = arc.read_or_err("session")?;
            let state = s.handle.current_state();
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
                sender: s.identity_display.to_string(),
                conversation_name: s.conversation_name.clone(),
            }
            .into();

            let packet = s.handle.expect_mls()?.build_message(&app_msg, &s.app_id)?;
            (Arc::clone(s.transport()), packet)
        };
        send_packet(&transport, packet)?;
        Ok(())
    }

    /// Start a `RemoveMember` consensus round targeting `ban_request.user_to_ban`.
    /// The requester's click means "I want this person removed" → the
    /// creator's vote is bundled as YES at submit; no banner is shown to
    /// the requester.
    pub async fn process_ban_request(
        arc: &Arc<RwLock<Self>>,
        ban_request: BanRequest,
    ) -> Result<(), UserError> {
        {
            let s = arc.read_or_err("session")?;
            let state = s.handle.current_state();
            if state != ConversationState::Working {
                return Err(UserError::ConversationBlocked(state.to_string()));
            }
        }

        Self::initiate_proposal(
            arc,
            ConversationUpdateRequest {
                payload: Some(conversation_update_request::Payload::RemoveMember(
                    RemoveMember {
                        identity: parse_wallet_to_bytes(ban_request.user_to_ban.as_str())?,
                    },
                )),
            },
            CreatorVote::Yes,
        )
        .await?;

        Ok(())
    }
}
