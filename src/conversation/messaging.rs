//! Send operations on `Conversation`: app messages, ban requests, and
//! member-add proposals.
//!
//! Also defines [`Outbound`] — the conversation's I/O-agnostic product.

use std::error::Error as StdError;

use openmls_traits::{OpenMlsProvider, signatures::Signer, storage::StorageProvider};

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationState, CreatorVote,
    PeerScoreStorage, WallClock,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationMessage, ConversationSyncRequest, ConversationUpdateRequest,
        MemberInvite,
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

impl<Cp, Sc, Wc> Conversation<Cp, Sc, Wc>
where
    Cp: ConsensusPlugin,
    Sc: PeerScoreStorage,
    Wc: WallClock,
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
        signer: &impl Signer,
        message: Vec<u8>,
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
            conversation_id: self.conversation_id.clone(),
        }
        .into();
        let payload = self.mls_mut().build_message(provider, signer, &app_msg)?;
        self.broadcast(payload);
        Ok(())
    }

    /// Broadcast a `ConversationSyncRequest` so a steward re-sends the `ConversationSync`;
    /// `poll` calls this automatically whenever the local steward list is missing or
    /// exhausted, paced at `backup_takeover_window` (the answer latency — a
    /// backup steward covers a silent epoch steward only after that window);
    /// [`crate::ConversationEvent::ConversationSyncApplied`] fires once a sync is adopted.
    pub fn request_conversation_sync<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let app_msg: AppMessage = ConversationSyncRequest {}.into();
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
        signer: &impl Signer,
        joiner_id: &[u8],
        key_package_bytes: &[u8],
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

    /// Sponsor a joiner who sent their own key package, but don't endorse it: just relay the request.
    /// Only the current epoch steward actually submits the Add proposal right away;
    /// everyone else saves it to propose later if the steward goes
    /// quiet (handled by `poll` after `backup_takeover_window`).
    /// Does nothing unless we're in `Working` state.
    ///
    /// For out-of-band invites with endorsement, use [`Self::add_member`].
    ///
    /// `joiner_id` is the serialized credential related to the `key_package_bytes`.
    pub fn sponsor_member<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        joiner_id: &[u8],
        key_package_bytes: &[u8],
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
        let epoch = self.mls().current_epoch()?;
        self.queues.insert_pending_update(
            ConversationUpdateRequest::member_invite(MemberInvite {
                key_package_bytes: key_package_bytes.to_vec(),
                credential: joiner_id.to_vec(),
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
        joiner_id: &[u8],
        creator_vote: CreatorVote,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.initiate_proposal(
            provider,
            ConversationUpdateRequest::member_invite(MemberInvite {
                key_package_bytes: key_package_bytes.to_vec(),
                credential: joiner_id.to_vec(),
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
        signer: &impl Signer,
        member_id: &[u8],
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
