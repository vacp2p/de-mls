//! OpenMLS-backed implementation of [`super::MlsService`].
//!
//! The trait definition lives in `super::api`; this file is the reference
//! impl for the OpenMLS engine. It works with any `DeMlsStorage` whose
//! `MlsStorage` implements [`openmls_traits::storage::StorageProvider`] —
//! the impl is not tied to `openmls_rust_crypto::MemoryStorage`.

use openmls::{
    group::MlsGroup,
    key_packages::KeyPackageIn,
    prelude::{
        ContentType, DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, ProcessedMessageContent,
        Proposal, ProtocolMessage, ProtocolVersion,
    },
};
use openmls_rust_crypto::RustCrypto;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use prost::Message;

use crate::{
    mls_crypto::{
        CommitCandidate, DeMlsStorage, DecryptResult, MlsCommitInput, MlsError, MlsMessageKind,
        MlsProposalOutput, OpenMlsService, StagedCandidateResult, service::api::MlsService,
    },
    protos::de_mls::messages::v1::AppMessage,
};

/// Internal OpenMLS provider that wraps the configured storage backend.
pub(super) struct MlsProvider<'a, T: StorageProvider<1>> {
    crypto: &'a RustCrypto,
    storage: &'a T,
}

impl<'a, T: StorageProvider<1>> MlsProvider<'a, T> {
    pub(super) fn new(crypto: &'a RustCrypto, storage: &'a T) -> Self {
        Self { crypto, storage }
    }
}

impl<'a, T: StorageProvider<1>> OpenMlsProvider for MlsProvider<'a, T> {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = T;

    fn crypto(&self) -> &Self::CryptoProvider {
        self.crypto
    }

    fn rand(&self) -> &Self::RandProvider {
        self.crypto
    }

    fn storage(&self) -> &Self::StorageProvider {
        self.storage
    }
}

impl<S> OpenMlsService<S>
where
    S: DeMlsStorage,
{
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
}

impl<S> MlsService for OpenMlsService<S>
where
    S: DeMlsStorage,
{
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    // ══════════════════════════════════════════════════════════
    // Conversation lifecycle
    // ══════════════════════════════════════════════════════════

    fn delete(&mut self) -> Result<(), MlsError> {
        self.group
            .delete(self.storage.mls_storage())
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
        updates: &[MlsCommitInput],
    ) -> Result<CommitCandidate, MlsError> {
        let crypto = &self.crypto;
        let mls_storage = self.storage.mls_storage();
        let provider = MlsProvider::new(crypto, mls_storage);
        let signer = self.credentials.signer();
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
                        group.propose_add_member(&provider, signer, &kp)?;
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
                            group.propose_remove_member(&provider, signer, index)?;
                        mls_proposals.push(mls_message_out.to_bytes()?);
                    }
                }
            }
        }

        let (commit_msg, welcome, _group_info) =
            group.commit_to_pending_proposals(&provider, signer)?;

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
        let crypto = &self.crypto;
        let mls_storage = self.storage.mls_storage();
        let provider = MlsProvider::new(crypto, mls_storage);
        self.group.merge_pending_commit(&provider)?;
        Ok(())
    }

    fn discard_own_commit(&mut self) -> Result<(), MlsError> {
        let crypto = &self.crypto;
        let mls_storage = self.storage.mls_storage();
        let provider = MlsProvider::new(crypto, mls_storage);
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
        let provider = MlsProvider::new(&self.crypto, self.storage.mls_storage());
        let group = &mut self.group;
        let conversation_id = &self.conversation_id;

        // ── Stage every proposal, collecting senders ──
        let mut proposal_senders: Vec<Vec<u8>> = Vec::with_capacity(proposals.len());
        for (i, proposal_bytes) in proposals.iter().enumerate() {
            let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(proposal_bytes)?;
            let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;
            let processed = group.process_message(&provider, protocol_message)?;
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

        let processed = group.process_message(&provider, protocol_message)?;
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
        let provider = MlsProvider::new(&self.crypto, self.storage.mls_storage());
        let staged = self
            .pending_staged_commit
            .take()
            .ok_or_else(|| MlsError::NoPendingStagedCommit(self.conversation_id.clone()))?;
        self.group.merge_staged_commit(&provider, staged)?;
        Ok(())
    }

    fn discard_staged_commit(&mut self) -> Result<(), MlsError> {
        self.pending_staged_commit = None;
        self.group
            .clear_pending_proposals(self.storage.mls_storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Application messages
    // ══════════════════════════════════════════════════════════

    fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, MlsError> {
        let provider = MlsProvider::new(&self.crypto, self.storage.mls_storage());
        let signer = self.credentials.signer();
        let message = self.group.create_message(&provider, signer, plaintext)?;
        Ok(message.to_bytes()?)
    }

    fn build_message(&mut self, app_msg: &AppMessage) -> Result<Vec<u8>, MlsError> {
        self.encrypt(&app_msg.encode_to_vec())
    }

    fn decrypt_application_only(&mut self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
        let provider = MlsProvider::new(&self.crypto, self.storage.mls_storage());
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

        let processed = group.process_message(&provider, protocol_message)?;
        let sender_id = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                Ok(DecryptResult::Application(app.into_bytes(), sender_id))
            }
            _ => Ok(DecryptResult::Ignored),
        }
    }

    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
        let provider = MlsProvider::new(&self.crypto, self.storage.mls_storage());
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

        let processed = group.process_message(&provider, protocol_message)?;
        let sender_id = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                Ok(DecryptResult::Application(app.into_bytes(), sender_id))
            }
            ProcessedMessageContent::ProposalMessage(proposal) => {
                let action =
                    OpenMlsService::<S>::extract_proposal_action(group, proposal.proposal())?;

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
