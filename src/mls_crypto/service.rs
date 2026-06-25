//! [`MlsService`] — DE-MLS's per-conversation MLS engine, backed by OpenMLS.
//!
//! Wraps one `MlsGroup` and exposes just the MLS operations the conversation
//! needs: seeding or joining the group, the commit pipeline (build → stage →
//! merge/discard), encrypting and decrypting traffic, and membership queries.
//! Results cross the boundary as the de-mls byte types in [`crate::mls_crypto`],
//! so the rest of the library speaks de-mls types, not OpenMLS ones.
//!
//! Provider, signer, and credential arrive by reference per call — one provider
//! can back every conversation. The service keeps only the group and a
//! staged-commit slot; the integrator owns identity, keys, and storage.

use std::error::Error as StdError;

use openmls::credentials::CredentialWithKey;
use openmls::group::{
    GroupId, MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig, StagedCommit, StagedWelcome,
    WelcomeError,
};
use openmls::key_packages::KeyPackageIn;
use openmls::prelude::{
    Ciphersuite, ContentType, DeserializeBytes, MlsMessageBodyIn, MlsMessageIn,
    ProcessedMessageContent, ProtocolMessage, ProtocolVersion,
};
use openmls_traits::storage::StorageProvider;
use openmls_traits::{OpenMlsProvider, signatures::Signer};
use prost::Message;

use crate::{
    mls_crypto::{
        CommitArtifacts, DecryptedMessage, MlsCommitInput, MlsError, MlsMessageKind,
        MlsProposalOutput, StagedCandidateResult,
    },
    protos::de_mls::messages::v1::AppMessage,
};

/// DE-MLS's MLS engine for one conversation: a single `MlsGroup` plus the
/// staged-commit slot for the inbound stage→merge/discard pipeline.
///
/// Provider, signer, and credential are passed in per call and never stored.
/// Read-only methods take `&self`; state-advancing ones take `&mut self`, and
/// callers serialize via the outer per-conversation lock.
pub struct MlsService {
    conversation_id: String,
    group: MlsGroup,
    pending_staged_commit: Option<StagedCommit>,
}

impl MlsService {
    // ══════════════════════════════════════════════════════════
    // Construction & teardown
    // ══════════════════════════════════════════════════════════

    /// Create a fresh group as its sole initial member ("creator"). The leaf is
    /// seeded straight from `credential` and `ciphersuite` — no key package,
    /// since key packages are how *joiners* are added. `provider` is written
    /// into but not retained.
    pub fn new_as_creator<Pr>(
        conversation_id: String,
        provider: &Pr,
        credential: CredentialWithKey,
        ciphersuite: Ciphersuite,
        signer: &impl Signer,
    ) -> Result<Self, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(ciphersuite)
            .use_ratchet_tree_extension(true)
            .build();
        let group = MlsGroup::new_with_group_id(
            provider,
            signer,
            &config,
            GroupId::from_slice(conversation_id.as_bytes()),
            credential,
        )?;

        Ok(Self {
            conversation_id,
            group,
            pending_staged_commit: None,
        })
    }

    /// Join a group from a welcome. `provider` must hold the joiner's
    /// key-package private keys from the earlier key-package build, and is read
    /// from but not retained. Returns `Ok(None)` when the welcome doesn't
    /// address one of our key packages — the "not for us" branch, not an error.
    pub fn new_from_welcome<Pr>(
        provider: &Pr,
        welcome_bytes: &[u8],
    ) -> Result<Option<Self>, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Ok(None),
        };

        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();
        let staged = match StagedWelcome::new_from_welcome(provider, &config, welcome, None) {
            Ok(staged) => staged,
            Err(WelcomeError::NoMatchingKeyPackage | WelcomeError::JoinerSecretNotFound) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let group = staged.into_group(provider)?;

        let conversation_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();
        Ok(Some(Self {
            conversation_id,
            group,
            pending_staged_commit: None,
        }))
    }

    /// Tear down all local MLS state for this conversation. Idempotent, so
    /// repeated leave / cleanup is safe.
    pub fn delete<Pr>(&mut self, provider: &Pr) -> Result<(), MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.group
            .delete(provider.storage())
            .map_err(MlsError::storage)
    }

    // ══════════════════════════════════════════════════════════
    // Queries
    // ══════════════════════════════════════════════════════════

    /// The conversation id this group is scoped to.
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// Current members as serialized credential bytes, one per leaf in MLS leaf
    /// order.
    pub fn members(&self) -> Result<Vec<Vec<u8>>, MlsError> {
        Ok(self
            .group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
            .collect())
    }

    /// Whether `member_id` is a current member.
    pub fn is_member(&self, member_id: &[u8]) -> bool {
        self.members()
            .map(|members| members.iter().any(|m| m.as_slice() == member_id))
            .unwrap_or(false)
    }

    /// Current MLS epoch — the single source of truth; keep no parallel counter.
    pub fn current_epoch(&self) -> Result<u64, MlsError> {
        Ok(self.group.epoch().as_u64())
    }

    // ══════════════════════════════════════════════════════════
    // Outbound commit: build our own, then resolve
    // ══════════════════════════════════════════════════════════

    /// Build a commit candidate from a batch of membership changes, returning
    /// the wire bytes (proposals, commit, and a welcome if anyone is added) for
    /// the steward to broadcast.
    ///
    /// The candidate is staged locally, not applied — another steward's commit
    /// may win the freeze round. Resolve it before the next MLS operation:
    /// [`merge_own_commit`](Self::merge_own_commit) if it wins,
    /// [`discard_own_commit`](Self::discard_own_commit) otherwise; a leftover
    /// pending commit blocks the next commit or message.
    pub fn create_commit_candidate<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        updates: &[MlsCommitInput],
    ) -> Result<CommitArtifacts, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let group = &mut self.group;
        let mut mls_proposals = Vec::new();

        for update in updates {
            match update {
                MlsCommitInput::Add(key_package_bytes) => {
                    let (kp_in, _rest) = KeyPackageIn::tls_deserialize_bytes(key_package_bytes)
                        .map_err(MlsError::KeyPackageTls)?;
                    let kp = kp_in
                        .validate(provider.crypto(), ProtocolVersion::Mls10)
                        .map_err(MlsError::storage)?;
                    let (mls_message_out, _proposal_ref) =
                        group.propose_add_member(provider, signer, &kp)?;
                    mls_proposals.push(mls_message_out.to_bytes()?);
                }
                MlsCommitInput::Remove(member_id) => {
                    let member_index = group.members().find_map(|m| {
                        if m.credential.serialized_content() == member_id {
                            Some(m.index)
                        } else {
                            None
                        }
                    });
                    if let Some(index) = member_index {
                        let (mls_message_out, _proposal_ref) =
                            group.propose_remove_member(provider, signer, index)?;
                        mls_proposals.push(mls_message_out.to_bytes()?);
                    }
                }
            }
        }

        let (commit_msg, welcome, _group_info) =
            group.commit_to_pending_proposals(provider, signer)?;

        let welcome_bytes = match welcome {
            Some(w) => Some(w.to_bytes()?),
            None => None,
        };

        Ok(CommitArtifacts {
            proposals: mls_proposals,
            commit: commit_msg.to_bytes()?,
            welcome: welcome_bytes,
        })
    }

    /// Apply our staged commit, advancing the epoch — call once our candidate
    /// has won the freeze round.
    pub fn merge_own_commit<Pr>(&mut self, provider: &Pr) -> Result<(), MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.group.merge_pending_commit(provider)?;
        Ok(())
    }

    /// Drop our staged commit and the pending proposals it carried — call when
    /// the candidate lost selection or we're rolling back.
    pub fn discard_own_commit<Pr>(&mut self, provider: &Pr) -> Result<(), MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.group
            .clear_pending_commit(provider.storage())
            .map_err(MlsError::storage)?;
        self.group
            .clear_pending_proposals(provider.storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Inbound commit: stage a peer's, then resolve
    // ══════════════════════════════════════════════════════════

    /// Stage a peer's commit candidate: each proposal is processed as pending,
    /// then the commit against them, leaving a staged commit held internally —
    /// not applied.
    ///
    /// The caller validates the result (sender, actions vs. the voted-approved
    /// set) and follows up with
    /// [`merge_staged_commit`](Self::merge_staged_commit) to advance the epoch
    /// or [`discard_staged_commit`](Self::discard_staged_commit) to roll back.
    /// See [`StagedCandidateResult`] for the outcomes; a benign
    /// [`Aborted`](StagedCandidateResult::Aborted) still needs a
    /// `discard_staged_commit` to clear partial state before the next candidate.
    pub fn stage_remote_commit<Pr>(
        &mut self,
        provider: &Pr,
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let group = &mut self.group;
        let conversation_id = &self.conversation_id;

        // ── Stage every proposal, collecting senders ──
        let mut proposal_senders: Vec<Vec<u8>> = Vec::with_capacity(proposals.len());
        for (i, proposal_bytes) in proposals.iter().enumerate() {
            let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(proposal_bytes)?;
            let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;
            let processed = group.process_message(provider, protocol_message)?;
            let sender = processed.credential().serialized_content().to_vec();
            match processed.into_content() {
                ProcessedMessageContent::ProposalMessage(proposal) => {
                    group
                        .store_pending_proposal(provider.storage(), proposal.as_ref().clone())
                        .map_err(MlsError::storage)?;
                    proposal_senders.push(sender);
                }
                _ => {
                    tracing::debug!(
                        group = %conversation_id,
                        index = i,
                        "stage_remote_commit: non-proposal in proposal slot",
                    );
                    return Ok(StagedCandidateResult::Aborted);
                }
            }
        }

        // ── Stage the commit ──
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(commit_bytes)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            tracing::debug!(
                "stage_remote_commit: ignoring commit for wrong group ID (expected {})",
                conversation_id,
            );
            return Ok(StagedCandidateResult::Aborted);
        }
        if protocol_message.epoch() < group.epoch() {
            tracing::debug!(
                "stage_remote_commit: ignoring stale commit from epoch {} (current: {})",
                protocol_message.epoch().as_u64(),
                group.epoch().as_u64(),
            );
            return Ok(StagedCandidateResult::Aborted);
        }

        let processed = group.process_message(provider, protocol_message)?;
        let commit_sender = processed.credential().serialized_content().to_vec();

        let outcome = match processed.into_content() {
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                let self_removed = staged.self_removed();
                let mut actions = Vec::new();
                for add in staged.add_proposals() {
                    let id = add
                        .add_proposal()
                        .key_package()
                        .leaf_node()
                        .credential()
                        .serialized_content()
                        .to_vec();
                    actions.push(MlsProposalOutput::Add(id));
                }
                for remove in staged.remove_proposals() {
                    let removed_index = remove.remove_proposal().removed();
                    let id = group
                        .member(removed_index)
                        .map(|c| c.serialized_content().to_vec())
                        .ok_or(MlsError::UnknownLeafIndex(removed_index.u32()))?;
                    actions.push(MlsProposalOutput::Remove(id));
                }
                Some((commit_sender, self_removed, actions, *staged))
            }
            _ => {
                tracing::debug!(
                    "stage_remote_commit: ignoring non-commit message for group {}",
                    conversation_id,
                );
                None
            }
        };

        match outcome {
            Some((commit_sender, self_removed, actions, staged)) => {
                // de-mls invariant: every bundled proposal must come from the
                // committer. MLS allows reference-by-id of others' proposals;
                // we don't.
                if proposal_senders.iter().any(|s| s != &commit_sender) {
                    return Ok(StagedCandidateResult::BundleSenderMismatch { commit_sender });
                }
                self.pending_staged_commit = Some(staged);
                Ok(StagedCandidateResult::Staged {
                    commit_sender,
                    self_removed,
                    actions,
                })
            }
            None => Ok(StagedCandidateResult::Aborted),
        }
    }

    /// Apply the staged peer commit, advancing the epoch. Errors if nothing is
    /// staged.
    pub fn merge_staged_commit<Pr>(&mut self, provider: &Pr) -> Result<(), MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let staged = self
            .pending_staged_commit
            .take()
            .ok_or_else(|| MlsError::NoPendingStagedCommit(self.conversation_id.clone()))?;
        self.group.merge_staged_commit(provider, staged)?;
        Ok(())
    }

    /// Drop the staged peer commit and the pending proposals it staged on top
    /// of.
    pub fn discard_staged_commit<Pr>(&mut self, provider: &Pr) -> Result<(), MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.pending_staged_commit = None;
        self.group
            .clear_pending_proposals(provider.storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Application messages
    // ══════════════════════════════════════════════════════════

    /// Encode and encrypt an [`AppMessage`], returning the raw payload bytes —
    /// the convenience path most senders use.
    pub fn build_message<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        app_msg: &AppMessage,
    ) -> Result<Vec<u8>, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let message = self
            .group
            .create_message(provider, signer, &app_msg.encode_to_vec())?;
        Ok(message.to_bytes()?)
    }

    /// Strict decrypt: accept only application messages, ignoring everything
    /// else (proposals and commits included). Guards the application subtopic
    /// against MLS-state pollution from peers that misroute control messages.
    ///
    /// Returns the [`DecryptedMessage`] for a current-epoch application message,
    /// or `None` when the message can't be taken as one (a proposal, commit, or
    /// wrong group/epoch is dropped, not an error).
    pub fn decrypt_application_only<Pr>(
        &mut self,
        provider: &Pr,
        ciphertext: &[u8],
    ) -> Result<Option<DecryptedMessage>, MlsError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let group = &mut self.group;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            return Ok(None);
        }

        // OpenMLS rejects both old and future epochs; ignore both to avoid
        // hard errors (a joiner sends at epoch N+1 before we've merged).
        if protocol_message.epoch() != group.epoch() {
            return Ok(None);
        }

        // Reject commits/proposals before process_message to avoid MLS errors
        // (e.g. MissingProposal when commit's proposals aren't stored).
        match protocol_message.content_type() {
            ContentType::Commit | ContentType::Proposal => {
                return Ok(None);
            }
            ContentType::Application => {}
        }

        let processed = group.process_message(provider, protocol_message)?;
        let sender_id = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(Some(DecryptedMessage {
                payload: app.into_bytes(),
                sender: sender_id,
            })),
            _ => Ok(None),
        }
    }

    /// Peek a wire message's outer kind without processing or signature-checking
    /// it — a cheap pre-dispatch lane check (e.g. "proposal or commit?").
    pub fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind, MlsError> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(message_bytes)?;
        let protocol = match mls_message.extract() {
            MlsMessageBodyIn::PrivateMessage(m) => ProtocolMessage::PrivateMessage(m),
            MlsMessageBodyIn::PublicMessage(m) => ProtocolMessage::PublicMessage(Box::new(m)),
            _ => return Ok(MlsMessageKind::Other),
        };

        let kind = match protocol.content_type() {
            ContentType::Proposal => MlsMessageKind::Proposal,
            ContentType::Commit => MlsMessageKind::Commit,
            ContentType::Application => MlsMessageKind::Other,
        };
        Ok(kind)
    }
}
