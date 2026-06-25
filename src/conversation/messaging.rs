//! Send operations on `Conversation`: app messages, ban requests, and
//! member-add proposals.
//!
//! Also defines [`Outbound`] — the conversation's I/O-agnostic product.

use std::error::Error as StdError;

use openmls_traits::{OpenMlsProvider, signatures::Signer, storage::StorageProvider};
use tracing::info;

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationState, CreatorVote,
    PeerScoreStorage,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationMessage, ConversationUpdateRequest, MemberInvite,
    },
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

impl<C, Sc> Conversation<C, Sc>
where
    C: ConsensusPlugin,
    Sc: PeerScoreStorage,
{
    /// Buffer a chat message for broadcast. The conversation never sends — the
    /// message is enqueued and the integrator drains it via
    /// [`Conversation::drain_outbound`]. Blocked in `Freezing` and `Selection`
    /// (epoch rotation in flight — the message might not decrypt on peers who
    /// already merged the next commit). Governance traffic has its own gate
    /// (`check_proposal_allowed`). `signer` is the local member's MLS signer,
    /// used to authenticate the outbound message.
    pub fn send_message<Pr>(
        &mut self,
        provider: &Pr,
        message: Vec<u8>,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let state = self.current_state();
        if matches!(
            state,
            ConversationState::Freezing | ConversationState::Selection
        ) {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }

        let app_msg: AppMessage = ConversationMessage {
            message,
            sender: self.self_member_id.to_vec(),
            conversation_id: self.conversation_id.clone(),
            ..Default::default()
        }
        .into();
        let payload = self.mls_mut().build_message(provider, signer, &app_msg)?;
        self.broadcast(payload);
        Ok(())
    }

    /// Invite a joiner whose key package the caller supplies out of band,
    /// endorsing the add by bundling a YES vote at submit. Any member may call.
    /// Errors unless the conversation is `Working`.
    ///
    /// `joiner_id` is the joiner's id, which the caller already holds (it is the
    /// credential the key package carries);
    ///
    /// See [`Self::sponsor_member`] for the non-endorsing steward relay.
    pub fn add_member<Pr>(
        &mut self,
        provider: &Pr,
        key_package_bytes: &[u8],
        joiner_id: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let state = self.current_state();
        if state != ConversationState::Working {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }
        self.propose_add(
            provider,
            key_package_bytes,
            joiner_id,
            CreatorVote::Yes,
            signer,
        )
    }

    /// Relay a joiner that announced its own key package, without endorsing it:
    /// the proposal is submitted unbundled ([`CreatorVote::Deferred`]) and this
    /// member votes on it like any other. Only the primary epoch steward relays
    /// immediately, so a single Add proposal is opened per joiner. Every other
    /// member records the announcement in the pending-update buffer instead —
    /// a backup proposes it from there if the epoch steward stays silent past
    /// the recovery window (drained by `poll`), so an offline epoch steward
    /// doesn't strand the join. No-op outside `Working`.
    ///
    /// See [`Self::add_member`] for the endorsing out-of-band invite.
    ///
    /// `joiner_id` is the announced joiner's id, taken from the `MemberInvite`
    /// the caller decoded (it travels alongside the key-package bytes on the
    /// wire);
    pub fn sponsor_member<Pr>(
        &mut self,
        provider: &Pr,
        key_package_bytes: &[u8],
        joiner_id: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if self.current_state() != ConversationState::Working {
            return Ok(());
        }
        if self.is_epoch_steward()? {
            return self.propose_add(
                provider,
                key_package_bytes,
                joiner_id,
                CreatorVote::Deferred,
                signer,
            );
        }
        if joiner_id == self.member_id_bytes() || self.mls().is_member(joiner_id) {
            return Ok(());
        }
        let epoch = self.mls().current_epoch()?;
        self.queues.insert_pending_update(
            ConversationUpdateRequest::member_invite(MemberInvite {
                key_package_bytes: key_package_bytes.to_vec(),
                member_id: joiner_id.to_vec(),
            }),
            epoch,
        );
        Ok(())
    }

    /// Shared body of [`Self::add_member`] / [`Self::sponsor_member`]: parse the
    /// key package, skip our own and already-present members, and open the Add
    /// proposal with the caller-chosen vote mode.
    fn propose_add<Pr>(
        &mut self,
        provider: &Pr,
        key_package_bytes: &[u8],
        member_id: &[u8],
        creator_vote: CreatorVote,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        // Don't propose our own key package.
        if member_id == self.member_id_bytes() {
            return Ok(());
        }
        // The target is already in the group — nothing to add.
        if self.mls().is_member(member_id) {
            info!(
                conversation = %self.id(),
                member = ?member_id,
                "add member skipped: already a member"
            );
            return Ok(());
        }
        self.initiate_proposal(
            provider,
            ConversationUpdateRequest::member_invite(MemberInvite {
                key_package_bytes: key_package_bytes.to_vec(),
                member_id: member_id.to_vec(),
            }),
            creator_vote,
            signer,
        )?;
        Ok(())
    }

    /// Start a `RemoveMember` consensus round targeting `member_id`. The
    /// requester's intent is the removal → the creator's vote is bundled as
    /// YES at submit; no vote request is shown to the requester.
    pub fn remove_member<Pr>(
        &mut self,
        provider: &Pr,
        member_id: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let state = self.current_state();
        if state != ConversationState::Working {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }

        self.initiate_proposal(
            provider,
            ConversationUpdateRequest::remove_member(member_id.to_vec()),
            CreatorVote::Yes,
            signer,
        )?;

        Ok(())
    }
}
