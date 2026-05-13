//! OpenMLS-backed implementation of [`super::MlsService`].
//!
//! The trait definition lives in `super::api`; this file is the reference
//! impl for the OpenMLS engine. It works with any `DeMlsStorage` whose
//! `MlsStorage` implements [`openmls_traits::storage::StorageProvider`] —
//! the impl is not tied to `openmls_rust_crypto::MemoryStorage`.

use openmls::{
    group::MlsGroup,
    prelude::{
        ContentType, DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, ProcessedMessageContent,
        Proposal, ProtocolMessage,
    },
};
use openmls_rust_crypto::RustCrypto;
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use prost::Message;

use crate::{
    ds::{APP_MSG_SUBTOPIC, OutboundPacket},
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
    fn provider(&self) -> MlsProvider<'_, S::MlsStorage> {
        MlsProvider {
            crypto: &self.crypto,
            storage: self.storage.mls_storage(),
        }
    }

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
            other => Ok(MlsProposalOutput::Other(format!("{other:?}"))),
        }
    }
}

impl<S> MlsService for OpenMlsService<S>
where
    S: DeMlsStorage + Send + Sync + 'static,
{
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    // ══════════════════════════════════════════════════════════
    // Conversation lifecycle
    // ══════════════════════════════════════════════════════════

    fn delete(&self) -> Result<(), MlsError> {
        let provider = self.provider();
        self.state
            .write()?
            .group
            .delete(provider.storage())
            .map_err(MlsError::storage)
    }

    // ══════════════════════════════════════════════════════════
    // Membership / state queries
    // ══════════════════════════════════════════════════════════

    fn members(&self) -> Result<Vec<Vec<u8>>, MlsError> {
        Ok(self
            .state
            .read()?
            .group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
            .collect())
    }

    fn is_member(&self, identity: &[u8]) -> bool {
        self.members()
            .map(|members| members.iter().any(|m| m.as_slice() == identity))
            .unwrap_or(false)
    }

    fn current_epoch(&self) -> Result<u64, MlsError> {
        Ok(self.state.read()?.group.epoch().as_u64())
    }

    // ══════════════════════════════════════════════════════════
    // Local commit pipeline (steward)
    // ══════════════════════════════════════════════════════════

    fn create_commit_candidate(
        &self,
        updates: &[MlsCommitInput],
    ) -> Result<CommitCandidate, MlsError> {
        let provider = self.provider();
        let signer = self.credentials.signer();

        let mut state = self.state.write()?;
        let group = &mut state.group;
        let mut mls_proposals = Vec::new();

        for update in updates {
            match update {
                MlsCommitInput::Add(key_package) => {
                    let kp: openmls::key_packages::KeyPackage =
                        serde_json::from_slice(key_package.as_bytes())
                            .map_err(MlsError::KeyPackageJson)?;
                    let (mls_message_out, _proposal_ref) =
                        group.propose_add_member(&provider, signer, &kp)?;
                    mls_proposals.push(mls_message_out.to_bytes()?);
                }
                MlsCommitInput::Remove(wallet_bytes) => {
                    let member_index = group.members().find_map(|m| {
                        if m.credential.serialized_content() == wallet_bytes {
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

    fn merge_own_commit(&self) -> Result<(), MlsError> {
        let provider = self.provider();
        self.state.write()?.group.merge_pending_commit(&provider)?;
        Ok(())
    }

    fn discard_own_commit(&self) -> Result<(), MlsError> {
        let provider = self.provider();
        let mut state = self.state.write()?;
        state
            .group
            .clear_pending_commit(provider.storage())
            .map_err(MlsError::storage)?;
        state
            .group
            .clear_pending_proposals(provider.storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Inbound candidate pipeline (stage → merge/discard)
    // ══════════════════════════════════════════════════════════

    fn stage_remote_commit(
        &self,
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError> {
        let provider = self.provider();

        // Hold the state lock for the whole atomic stage. Anything we put
        // into MLS pending state stays there for the caller to roll back
        // via `discard_staged_commit` on Aborted; the pending-staged-commit
        // slot is written before the lock drops so observers never see the
        // group at the post-stage epoch with `pending_staged_commit = None`.
        let mut state = self.state.write()?;
        let outcome = {
            let group = &mut state.group;

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
                            group = %self.conversation_id,
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
                    self.conversation_id,
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

            match processed.into_content() {
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
                    Some((
                        commit_sender,
                        proposal_senders,
                        self_removed,
                        actions,
                        *staged,
                    ))
                }
                _ => {
                    tracing::debug!(
                        "stage_remote_commit: ignoring non-commit message for group {}",
                        self.conversation_id,
                    );
                    None
                }
            }
        };

        match outcome {
            Some((commit_sender, proposal_senders, self_removed, actions, staged)) => {
                state.pending_staged_commit = Some(staged);
                Ok(StagedCandidateResult::Staged {
                    commit_sender,
                    proposal_senders,
                    self_removed,
                    actions,
                })
            }
            None => Ok(StagedCandidateResult::Aborted),
        }
    }

    fn merge_staged_commit(&self) -> Result<(), MlsError> {
        let provider = self.provider();
        let mut state = self.state.write()?;
        let staged = state
            .pending_staged_commit
            .take()
            .ok_or_else(|| MlsError::NoPendingStagedCommit(self.conversation_id.clone()))?;
        state.group.merge_staged_commit(&provider, staged)?;
        Ok(())
    }

    fn discard_staged_commit(&self) -> Result<(), MlsError> {
        let provider = self.provider();
        let mut state = self.state.write()?;
        state.pending_staged_commit = None;
        state
            .group
            .clear_pending_proposals(provider.storage())
            .map_err(MlsError::storage)?;
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Application messages
    // ══════════════════════════════════════════════════════════

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, MlsError> {
        let provider = self.provider();
        let signer = self.credentials.signer();
        let message = self
            .state
            .write()?
            .group
            .create_message(&provider, signer, plaintext)?;
        Ok(message.to_bytes()?)
    }

    fn build_message(
        &self,
        app_msg: &AppMessage,
        app_id: &[u8],
    ) -> Result<OutboundPacket, MlsError> {
        let bytes = self.encrypt(&app_msg.encode_to_vec())?;
        Ok(OutboundPacket::new(
            bytes,
            APP_MSG_SUBTOPIC,
            self.conversation_id(),
            app_id,
        ))
    }

    fn decrypt_application_only(&self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
        let provider = self.provider();

        let mut state = self.state.write()?;
        let group = &mut state.group;

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
        let sender_identity = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(DecryptResult::Application(
                app.into_bytes(),
                sender_identity,
            )),
            _ => Ok(DecryptResult::Ignored),
        }
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
        let provider = self.provider();

        let mut state = self.state.write()?;
        let group = &mut state.group;

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
                self.conversation_id,
            );
            return Ok(DecryptResult::Ignored);
        }

        let processed = group.process_message(&provider, protocol_message)?;
        let sender_identity = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => Ok(DecryptResult::Application(
                app.into_bytes(),
                sender_identity,
            )),
            ProcessedMessageContent::ProposalMessage(proposal) => {
                let action =
                    OpenMlsService::<S>::extract_proposal_action(group, proposal.proposal())?;

                group
                    .store_pending_proposal(provider.storage(), proposal.as_ref().clone())
                    .map_err(MlsError::storage)?;
                Ok(DecryptResult::ProposalStored(sender_identity, action))
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
