//! Send operations on `SessionRunner`: app messages and ban requests.
//! Key-package announcement is a user-level concern (the conversation knows
//! nothing about how a key package is built) and lives on [`crate::app::User`].
//!
//! Also defines [`Outbound`] — the conversation's I/O-agnostic product.

use crate::{
    app::{ConversationState, CreatorVote, SessionRunner, SessionTick, UserError},
    core::{ConsensusPlugin, ConversationPluginsFactory},
    ds::OutboundPacket,
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, ConversationMessage, ConversationUpdateRequest, RemoveMember,
        conversation_update_request,
    },
};

/// A payload the conversation produced for the integrator to broadcast,
/// tagged with the conversation it belongs to and the local sender (for
/// self-message filtering). Already-encrypted bytes plus pragmatic
/// addressing — no transport subtopic. The session never sends: it buffers
/// these and the integrator drains them via
/// [`SessionRunner::drain_outbound`], converting each to a wire
/// [`OutboundPacket`] (the conversation only ever emits broadcast traffic —
/// chat, votes, sync, commit candidates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    pub conversation_id: String,
    pub sender: Vec<u8>,
    pub payload: Vec<u8>,
}

impl From<Outbound> for OutboundPacket {
    fn from(out: Outbound) -> Self {
        OutboundPacket::broadcast(&out.conversation_id, &out.sender, out.payload)
    }
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Buffer a chat message for broadcast. The session never sends — the
    /// message is enqueued and the integrator drains it via
    /// [`SessionRunner::drain_outbound`]. Blocked in `PendingJoin` (no keys
    /// yet), `Freezing`, and `Selection` (epoch rotation in flight — the
    /// message might not decrypt on peers who already merged the next
    /// commit). Governance traffic has its own gate (`check_proposal_allowed`).
    pub fn push_message(&mut self, message: Vec<u8>) -> Result<SessionTick, UserError> {
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
        let payload = self
            .conversation
            .expect_mls_mut()?
            .build_message(&app_msg)?;
        self.broadcast(payload);
        Ok(self.tick())
    }

    /// Start a `RemoveMember` consensus round targeting `ban_request.user_to_ban`.
    /// The requester's click means "I want this person removed" → the
    /// creator's vote is bundled as YES at submit; no vote request is shown to
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
