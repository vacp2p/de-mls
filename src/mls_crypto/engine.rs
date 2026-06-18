//! [`OpenMlsService`] — the OpenMLS-backed
//! [`MlsService`] engine: the per-conversation
//! struct, its `new_as_creator` / `new_from_welcome` constructors, and the full
//! trait implementation.
//!
//! Generic over the OpenMLS provider `P` (crypto + rand + storage), which it
//! owns, so it pins no storage or crypto backend — `&self.provider` and
//! `&mut self.group` are disjoint fields, so no borrow-splitting adapter is
//! needed.

use std::error::Error as StdError;

use openmls::credentials::CredentialWithKey;
use openmls::group::{
    GroupId, MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig, StagedCommit, StagedWelcome,
    WelcomeError,
};
use openmls::key_packages::KeyPackageIn;
use openmls::prelude::{
    Ciphersuite, ContentType, DeserializeBytes, MlsMessageBodyIn, MlsMessageIn,
    ProcessedMessageContent, Proposal, ProtocolMessage, ProtocolVersion,
};
use openmls_traits::storage::StorageProvider;
use openmls_traits::{OpenMlsProvider, signatures::Signer};
use prost::Message;

use crate::{
    mls_crypto::{
        CommitCandidate, DEFAULT_COMMIT_BATCH_MAX, DecryptResult, MlsCommitInput, MlsError,
        MlsMessageKind, MlsProposalOutput, MlsService, StagedCandidateResult,
    },
    protos::de_mls::messages::v1::AppMessage,
};

/// OpenMLS-backed MLS service, scoped to a single conversation.
///
/// Owns the OpenMLS provider `P` (crypto + rand + storage — e.g.
/// `openmls_rust_crypto::OpenMlsRustCrypto`), one `MlsGroup`, and an optional
/// staged-commit slot for the inbound stage→merge/discard pipeline. The
/// integrator chooses `P`; the engine pins no storage or crypto backend.
///
/// The service holds no identity material either: the credential and
/// ciphersuite arrive in the creator's key package at construction, and the
/// signer is passed in per call (see the
/// [`MlsService`] signing methods).
pub struct OpenMlsService<P: OpenMlsProvider> {
    provider: P,
    conversation_id: String,
    group: MlsGroup,
    pending_staged_commit: Option<StagedCommit>,
}

impl<P> OpenMlsService<P>
where
    P: OpenMlsProvider,
    <P::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    /// Create a fresh MLS group as the sole initial member ("creator").
    ///
    /// The creator seeds its leaf straight from `credential` and `ciphersuite`
    /// — it needs no key package (key packages are how *joiners* are added).
    pub fn new_as_creator(
        conversation_id: String,
        provider: P,
        credential: CredentialWithKey,
        ciphersuite: Ciphersuite,
        signer: &impl Signer,
    ) -> Result<Self, MlsError> {
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(ciphersuite)
            .use_ratchet_tree_extension(true)
            .build();
        let group = MlsGroup::new_with_group_id(
            &provider,
            signer,
            &config,
            GroupId::from_slice(conversation_id.as_bytes()),
            credential,
        )?;

        Ok(Self {
            provider,
            conversation_id,
            group,
            pending_staged_commit: None,
        })
    }

    /// Try to join a group from a serialized welcome, using `provider` — which
    /// must hold the joiner's key-package private keys from the earlier
    /// key-package build.
    ///
    /// Returns `Ok(None)` when the welcome doesn't address one of our key
    /// packages — the "not for us" branch, not an error — detected by the
    /// `NoMatchingKeyPackage` / `JoinerSecretNotFound` join errors.
    pub fn new_from_welcome(welcome_bytes: &[u8], provider: P) -> Result<Option<Self>, MlsError> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Ok(None),
        };

        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();
        let staged = match StagedWelcome::new_from_welcome(&provider, &config, welcome, None) {
            Ok(staged) => staged,
            Err(WelcomeError::NoMatchingKeyPackage | WelcomeError::JoinerSecretNotFound) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        let group = staged.into_group(&provider)?;

        let conversation_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();
        Ok(Some(Self {
            provider,
            conversation_id,
            group,
            pending_staged_commit: None,
        }))
    }
}

/// Read the membership change carried by a single MLS proposal.
fn extract_proposal_action(
    group: &MlsGroup,
    proposal: &Proposal,
) -> Result<MlsProposalOutput, MlsError> {
    match proposal {
        Proposal::Add(add) => {
            let id = add
                .key_package()
                .leaf_node()
                .credential()
                .serialized_content()
                .to_vec();
            Ok(MlsProposalOutput::Add(id))
        }
        Proposal::Remove(remove) => {
            let removed = remove.removed();
            let id = group
                .member(removed)
                .map(|c| c.serialized_content().to_vec())
                .ok_or(MlsError::UnknownLeafIndex(removed.u32()))?;
            Ok(MlsProposalOutput::Remove(id))
        }
        _ => Err(MlsError::UnknownProposalAction),
    }
}

impl<P> MlsService for OpenMlsService<P>
where
    P: OpenMlsProvider,
    <P::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn commit_batch_max(&self) -> usize {
        DEFAULT_COMMIT_BATCH_MAX
    }

    // ══════════════════════════════════════════════════════════
    // Conversation lifecycle
    // ══════════════════════════════════════════════════════════

    fn delete(&mut self) -> Result<(), MlsError> {
        self.group
            .delete(self.provider.storage())
            .map_err(MlsError::storage)
    }

    // ══════════════════════════════════════════════════════════
    // Membership / state queries
    // ══════════════════════════════════════════════════════════

    fn members(&self) -> Result<Vec<Vec<u8>>, MlsError> {
        Ok(self
            .group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
            .collect())
    }

    fn is_member(&self, member_id: &[u8]) -> bool {
        self.members()
            .map(|members| members.iter().any(|m| m.as_slice() == member_id))
            .unwrap_or(false)
    }

    fn current_epoch(&self) -> Result<u64, MlsError> {
        Ok(self.group.epoch().as_u64())
    }

    // ══════════════════════════════════════════════════════════
    // Local commit pipeline (steward)
    // ══════════════════════════════════════════════════════════

    fn create_commit_candidate(
        &mut self,
        signer: &impl Signer,
        updates: &[MlsCommitInput],
    ) -> Result<CommitCandidate, MlsError> {
        let provider = &self.provider;
        let group = &mut self.group;
        let mut mls_proposals = Vec::new();

        for update in updates {
            match update {
                MlsCommitInput::Add(key_package) => {
                    let (kp_in, _rest) =
                        KeyPackageIn::tls_deserialize_bytes(key_package.as_bytes())
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

        Ok(CommitCandidate {
            proposals: mls_proposals,
            commit: commit_msg.to_bytes()?,
            welcome: welcome_bytes,
        })
    }

    fn merge_own_commit(&mut self) -> Result<(), MlsError> {
        let provider = &self.provider;
        self.group.merge_pending_commit(provider)?;
        Ok(())
    }

    fn discard_own_commit(&mut self) -> Result<(), MlsError> {
        let provider = &self.provider;
        self.group
            .clear_pending_commit(provider.storage())
            .map_err(MlsError::storage)?;
        self.group
            .clear_pending_proposals(provider.storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Inbound candidate pipeline (stage → merge/discard)
    // ══════════════════════════════════════════════════════════

    fn stage_remote_commit(
        &mut self,
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError> {
        let provider = &self.provider;
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
                // de-mls invariant: every bundled proposal must come
                // from the committer. MLS allows reference-by-id of
                // others' proposals; we don't.
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

    fn merge_staged_commit(&mut self) -> Result<(), MlsError> {
        let provider = &self.provider;
        let staged = self
            .pending_staged_commit
            .take()
            .ok_or_else(|| MlsError::NoPendingStagedCommit(self.conversation_id.clone()))?;
        self.group.merge_staged_commit(provider, staged)?;
        Ok(())
    }

    fn discard_staged_commit(&mut self) -> Result<(), MlsError> {
        self.pending_staged_commit = None;
        self.group
            .clear_pending_proposals(self.provider.storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Application messages
    // ══════════════════════════════════════════════════════════

    fn encrypt(&mut self, signer: &impl Signer, plaintext: &[u8]) -> Result<Vec<u8>, MlsError> {
        let provider = &self.provider;
        let message = self.group.create_message(provider, signer, plaintext)?;
        Ok(message.to_bytes()?)
    }

    fn build_message(
        &mut self,
        signer: &impl Signer,
        app_msg: &AppMessage,
    ) -> Result<Vec<u8>, MlsError> {
        self.encrypt(signer, &app_msg.encode_to_vec())
    }

    fn decrypt_application_only(&mut self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
        let provider = &self.provider;
        let group = &mut self.group;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            return Ok(DecryptResult::Ignored);
        }

        // OpenMLS rejects both old and future epochs; ignore both to avoid
        // hard errors (a joiner sends at epoch N+1 before we've merged).
        if protocol_message.epoch() != group.epoch() {
            return Ok(DecryptResult::Ignored);
        }

        // Reject commits/proposals before process_message to avoid MLS errors
        // (e.g. MissingProposal when commit's proposals aren't stored).
        match protocol_message.content_type() {
            ContentType::Commit | ContentType::Proposal => {
                return Ok(DecryptResult::Ignored);
            }
            ContentType::Application => {}
        }

        let processed = group.process_message(provider, protocol_message)?;
        let sender_id = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                Ok(DecryptResult::Application(app.into_bytes(), sender_id))
            }
            _ => Ok(DecryptResult::Ignored),
        }
    }

    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
        let provider = &self.provider;
        let group = &mut self.group;
        let conversation_id = &self.conversation_id;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            return Ok(DecryptResult::Ignored);
        }

        // Old epochs can't be processed; future epochs arrive when a joiner
        // sends at epoch N+1 before we've merged our pending commit.
        if protocol_message.epoch() != group.epoch() {
            tracing::debug!(
                "Ignoring message from epoch {} (current: {})",
                protocol_message.epoch().as_u64(),
                group.epoch().as_u64()
            );
            return Ok(DecryptResult::Ignored);
        }

        if protocol_message.content_type() == ContentType::Commit {
            tracing::debug!(
                "Ignoring commit on decrypt() path for group {}: use stage_remote_commit() instead",
                conversation_id,
            );
            return Ok(DecryptResult::Ignored);
        }

        let processed = group.process_message(provider, protocol_message)?;
        let sender_id = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                Ok(DecryptResult::Application(app.into_bytes(), sender_id))
            }
            ProcessedMessageContent::ProposalMessage(proposal) => {
                let action = extract_proposal_action(group, proposal.proposal())?;

                group
                    .store_pending_proposal(provider.storage(), proposal.as_ref().clone())
                    .map_err(MlsError::storage)?;
                Ok(DecryptResult::ProposalStored(sender_id, action))
            }
            ProcessedMessageContent::StagedCommitMessage(_) => Ok(DecryptResult::Ignored),
            ProcessedMessageContent::ExternalJoinProposalMessage(_) => Ok(DecryptResult::Ignored),
        }
    }

    fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind, MlsError> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(message_bytes)?;
        let protocol = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(_) => return Ok(MlsMessageKind::Welcome),
            MlsMessageBodyIn::PrivateMessage(m) => ProtocolMessage::PrivateMessage(m),
            MlsMessageBodyIn::PublicMessage(m) => ProtocolMessage::PublicMessage(Box::new(m)),
            _ => return Ok(MlsMessageKind::Other),
        };

        let kind = match protocol.content_type() {
            ContentType::Application => MlsMessageKind::Application,
            ContentType::Proposal => MlsMessageKind::Proposal,
            ContentType::Commit => MlsMessageKind::Commit,
        };
        Ok(kind)
    }
}
