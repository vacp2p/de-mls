//! Send operations on `Conversation`: app messages, ban requests, and
//! member-add proposals.
//!
//! Also defines [`Outbound`] — the conversation's I/O-agnostic product, and
//! [`build_key_package_announcement`] — the encoding helper for KP broadcasts.

use openmls_traits::signatures::Signer;
use prost::Message;

use crate::{
    core::{ConsensusPlugin, ConversationPluginsFactory},
    mls_crypto::{KeyPackageBytes, MlsService, key_package_bytes_from_tls},
    protos::de_mls::messages::v1::{
        AppMessage, ConversationMessage, ConversationUpdateRequest, MemberInvite,
    },
    session::{Conversation, ConversationError, ConversationState, CreatorVote},
};

/// A payload the conversation produced for the integrator to broadcast,
/// tagged with the conversation it belongs to and the local sender (for
/// self-message filtering). Already-encrypted bytes plus pragmatic
/// addressing — no transport subtopic. The conversation never sends: it buffers
/// these and the integrator drains them via [`Conversation::drain_outbound`]
/// and maps each onto its own transport (the conversation only ever emits
/// broadcast traffic — chat, votes, sync, commit candidates). The reference
/// transport's `From<Outbound>` conversion lives in the `de-mls-ds` crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    pub conversation_id: String,
    pub sender: Vec<u8>,
    pub payload: Vec<u8>,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Conversation<P, CP> {
    /// Buffer a chat message for broadcast. The conversation never sends — the
    /// message is enqueued and the integrator drains it via
    /// [`Conversation::drain_outbound`]. Blocked in `PendingJoin` (no keys
    /// yet), `Freezing`, and `Selection` (epoch rotation in flight — the
    /// message might not decrypt on peers who already merged the next
    /// commit). Governance traffic has its own gate (`check_proposal_allowed`).
    /// `signer` is the local member's MLS signer, used to authenticate the
    /// outbound message.
    pub fn send_message(
        &mut self,
        message: Vec<u8>,
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        let state = self.current_state();
        if matches!(
            state,
            ConversationState::PendingJoin
                | ConversationState::Freezing
                | ConversationState::Selection
        ) {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }

        let app_msg: AppMessage = ConversationMessage {
            message,
            sender: self.member_id_display.to_string(),
            conversation_id: self.conversation_id.clone(),
        }
        .into();
        let payload = self.expect_mls_mut()?.build_message(signer, &app_msg)?;
        self.broadcast(payload);
        Ok(())
    }

    /// Start a `MemberInvite` consensus round for the given TLS-encoded key
    /// package. Parses the key package, extracts the member id, and submits a
    /// proposal with a bundled YES vote. The resulting welcome fires as
    /// [`crate::core::ConversationEvent::WelcomeReady`] for the integrator to
    /// deliver out of band.
    pub fn add_member(
        &mut self,
        key_package_bytes: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        let state = self.current_state();
        if state != ConversationState::Working {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }
        let (kp_bytes, member_id) = key_package_bytes_from_tls(key_package_bytes.to_vec())?;
        self.initiate_proposal(
            ConversationUpdateRequest::member_invite(MemberInvite {
                key_package_bytes: kp_bytes,
                member_id,
            }),
            CreatorVote::Yes,
            signer,
        )?;
        Ok(())
    }

    /// Start a `RemoveMember` consensus round targeting `member_id`. The
    /// requester's intent is the removal → the creator's vote is bundled as
    /// YES at submit; no vote request is shown to the requester.
    pub fn remove_member(
        &mut self,
        member_id: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        let state = self.current_state();
        if state != ConversationState::Working {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }

        self.initiate_proposal(
            ConversationUpdateRequest::remove_member(member_id.to_vec()),
            CreatorVote::Yes,
            signer,
        )?;

        Ok(())
    }
}

/// Encode a key package into the wire format used for KP announcements.
/// Returns the prost-encoded `MemberInvite` bytes ready for broadcast on the
/// welcome subtopic.
pub fn build_key_package_announcement(key_package: &KeyPackageBytes) -> Vec<u8> {
    MemberInvite {
        key_package_bytes: key_package.as_bytes().to_vec(),
        member_id: key_package.member_id().to_vec(),
    }
    .encode_to_vec()
}
