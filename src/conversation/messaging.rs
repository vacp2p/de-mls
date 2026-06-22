//! Send operations on `Conversation`: app messages, ban requests, and
//! member-add proposals.
//!
//! Also defines [`Outbound`] — the conversation's I/O-agnostic product, and
//! [`build_key_package_announcement`] — the encoding helper for KP broadcasts.

use std::error::Error as StdError;

use openmls_traits::signatures::Signer;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use prost::Message;
use tracing::info;

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationState, CreatorVote,
    PeerScoringPlugin, StewardListPlugin,
    mls_crypto::{KeyPackageBytes, MlsService, key_package_bytes_from_tls},
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

impl<C, Sc, St> Conversation<C, Sc, St>
where
    C: ConsensusPlugin,
    Sc: PeerScoringPlugin,
    St: StewardListPlugin,
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
    /// See [`Self::sponsor_member`] for the non-endorsing steward relay.
    pub fn add_member<Pr>(
        &mut self,
        provider: &Pr,
        key_package_bytes: &[u8],
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
        self.propose_add(provider, key_package_bytes, CreatorVote::Yes, signer)
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
    pub fn sponsor_member<Pr>(
        &mut self,
        provider: &Pr,
        key_package_bytes: &[u8],
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
            return self.propose_add(provider, key_package_bytes, CreatorVote::Deferred, signer);
        }
        self.buffer_announced_add(key_package_bytes)
    }

    /// Record an announced joiner in the pending-update buffer (the same buffer
    /// every member keeps for membership changes seen on the wire). Lets a
    /// backup steward propose the Add later if the epoch steward never does.
    /// Skips our own key package and members already in the group.
    fn buffer_announced_add(&mut self, key_package_bytes: &[u8]) -> Result<(), ConversationError> {
        let (kp_bytes, member_id) = key_package_bytes_from_tls(key_package_bytes.to_vec())?;
        if member_id == *self.member_id_bytes() || self.mls().is_member(&member_id) {
            return Ok(());
        }
        let epoch = self.mls().current_epoch()?;
        self.queues.insert_pending_update(
            ConversationUpdateRequest::member_invite(MemberInvite {
                key_package_bytes: kp_bytes,
                member_id,
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
        creator_vote: CreatorVote,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let (kp_bytes, member_id) = key_package_bytes_from_tls(key_package_bytes.to_vec())?;
        // Don't propose our own key package.
        if member_id == *self.member_id_bytes() {
            return Ok(());
        }
        // The target is already in the group — nothing to add.
        if self.mls().is_member(&member_id) {
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
                key_package_bytes: kp_bytes,
                member_id,
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
