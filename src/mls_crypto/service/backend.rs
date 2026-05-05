//! OpenMLS-backed implementation of [`super::MlsService`].
//!
//! The trait definition lives in `super::api`; this file is the reference
//! impl for the OpenMLS engine on `MemoryStorage`.

use openmls::{
    group::{GroupId, MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig, StagedWelcome},
    key_packages::KeyPackage,
    prelude::{
        ContentType, DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, ProcessedMessageContent,
        Proposal, ProtocolMessage,
    },
};
use openmls_rust_crypto::{MemoryStorage, RustCrypto};
use openmls_traits::OpenMlsProvider;

use crate::mls_crypto::{
    CommitCandidate, DeMlsStorage, DecryptResult, GroupUpdate, IdentityProvider, KeyPackageBytes,
    MlsError, MlsMessageKind, MlsProposalAction, OpenMlsService, StagedCandidateResult,
    service::api::{CIPHERSUITE, MlsService},
};

/// Internal OpenMLS provider that wraps storage. Used by
/// [`OpenMlsService::make_provider`].
struct MlsProvider<'a> {
    crypto: &'a RustCrypto,
    storage: &'a MemoryStorage,
}

impl<'a> OpenMlsProvider for MlsProvider<'a> {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = MemoryStorage;

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

impl<S, I> OpenMlsService<S, I>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
    I: IdentityProvider,
{
    fn make_provider(&self) -> MlsProvider<'_> {
        MlsProvider {
            crypto: &self.crypto,
            storage: self.storage.mls_storage(),
        }
    }

    fn extract_proposal_action(
        group: &MlsGroup,
        proposal: &Proposal,
    ) -> Result<MlsProposalAction, MlsError> {
        match proposal {
            Proposal::Add(add) => {
                let id = add
                    .key_package()
                    .leaf_node()
                    .credential()
                    .serialized_content()
                    .to_vec();
                Ok(MlsProposalAction::Add(id))
            }
            Proposal::Remove(remove) => {
                let removed = remove.removed();
                let id = group
                    .member(removed)
                    .map(|c| c.serialized_content().to_vec())
                    .ok_or(MlsError::UnknownLeafIndex(removed.u32()))?;
                Ok(MlsProposalAction::Remove(id))
            }
            other => Ok(MlsProposalAction::Other(format!("{other:?}"))),
        }
    }
}

impl<S, I> MlsService for OpenMlsService<S, I>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage> + Send + Sync + 'static,
    I: IdentityProvider,
{
    type Identity = I;

    fn identity(&self) -> &Self::Identity {
        &self.identity
    }

    // ══════════════════════════════════════════════════════════
    // Group lifecycle
    // ══════════════════════════════════════════════════════════

    fn create_group(&self, group_id: &str) -> Result<(), MlsError> {
        let provider = self.make_provider();

        let config = MlsGroupCreateConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let group = MlsGroup::new_with_group_id(
            &provider,
            self.identity.signer(),
            &config,
            GroupId::from_slice(group_id.as_bytes()),
            self.identity.credential().clone(),
        )?;

        self.groups.write()?.insert(group_id.to_string(), group);

        Ok(())
    }

    fn accept_welcome(&self, welcome_bytes: &[u8]) -> Result<Option<String>, MlsError> {
        let provider = self.make_provider();

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Ok(None),
        };

        let is_for_us = welcome.secrets().iter().any(|s| {
            self.storage
                .is_our_key_package(s.new_member().as_slice())
                .unwrap_or(false)
        });
        if !is_for_us {
            return Ok(None);
        }

        for secret in welcome.secrets() {
            let _ = self
                .storage
                .remove_key_package_ref(secret.new_member().as_slice());
        }

        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();
        let group = StagedWelcome::new_from_welcome(&provider, &config, welcome, None)?
            .into_group(&provider)?;

        let group_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();
        self.groups.write()?.insert(group_id.clone(), group);

        Ok(Some(group_id))
    }

    fn delete_group(&self, group_id: &str) -> Result<(), MlsError> {
        let provider = self.make_provider();
        let mut groups = self.groups.write()?;
        if let Some(mut group) = groups.remove(group_id) {
            group.delete(provider.storage())?;
        }
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Membership / state queries
    // ══════════════════════════════════════════════════════════

    fn members(&self, group_id: &str) -> Result<Vec<Vec<u8>>, MlsError> {
        let groups = self.groups.read()?;
        let group = groups
            .get(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

        Ok(group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
            .collect())
    }

    fn is_member(&self, group_id: &str, identity: &[u8]) -> bool {
        self.members(group_id)
            .map(|members| members.iter().any(|m| m.as_slice() == identity))
            .unwrap_or(false)
    }

    fn current_epoch(&self, group_id: &str) -> Result<u64, MlsError> {
        let groups = self.groups.read()?;
        let group = groups
            .get(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

        Ok(group.epoch().as_u64())
    }

    fn has_group(&self, group_id: &str) -> bool {
        self.groups
            .read()
            .map(|g| g.contains_key(group_id))
            .unwrap_or(false)
    }

    // ══════════════════════════════════════════════════════════
    // Key packages
    // ══════════════════════════════════════════════════════════

    fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        let provider = self.make_provider();

        let kp_bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            &provider,
            self.identity.signer(),
            self.identity.credential().clone(),
        )?;

        let kp = kp_bundle.key_package();
        let hash_ref = kp.hash_ref(provider.crypto())?.as_slice().to_vec();
        let bytes = serde_json::to_vec(kp).map_err(MlsError::InvalidJson)?;

        self.storage.store_key_package_ref(&hash_ref)?;

        Ok(KeyPackageBytes::new(
            bytes,
            self.identity.identity_bytes().to_vec(),
        ))
    }

    // ══════════════════════════════════════════════════════════
    // Local commit pipeline (steward)
    // ══════════════════════════════════════════════════════════

    fn create_commit_candidate(
        &self,
        group_id: &str,
        updates: &[GroupUpdate],
    ) -> Result<CommitCandidate, MlsError> {
        let provider = self.make_provider();
        let signer = self.identity.signer();

        let mut groups = self.groups.write()?;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

        let mut mls_proposals = Vec::new();

        for update in updates {
            match update {
                GroupUpdate::Add(key_package) => {
                    let kp: KeyPackage = serde_json::from_slice(key_package.as_bytes())
                        .map_err(MlsError::KeyPackageJson)?;
                    let (mls_message_out, _proposal_ref) =
                        group.propose_add_member(&provider, signer, &kp)?;
                    mls_proposals.push(mls_message_out.to_bytes()?);
                }
                GroupUpdate::Remove(wallet_bytes) => {
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

    fn merge_own_commit(&self, group_id: &str) -> Result<(), MlsError> {
        let provider = self.make_provider();

        let mut groups = self.groups.write()?;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

        group.merge_pending_commit(&provider)?;
        Ok(())
    }

    fn discard_own_commit(&self, group_id: &str) -> Result<(), MlsError> {
        let provider = self.make_provider();

        let mut groups = self.groups.write()?;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

        let _ = group.clear_pending_commit(provider.storage());
        let _ = group.clear_pending_proposals(provider.storage());
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Inbound candidate pipeline (stage → merge/discard)
    // ══════════════════════════════════════════════════════════

    fn apply_remote_candidate(
        &self,
        group_id: &str,
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError> {
        let provider = self.make_provider();

        // Phase 1: hold the groups lock for the whole atomic stage.
        // Anything we put into MLS pending state stays there for the caller
        // to roll back via `discard_staged_commit` on Aborted.
        let outcome = {
            let mut groups = self.groups.write()?;
            let group = groups
                .get_mut(group_id)
                .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

            // ── Stage every proposal, collecting senders ──
            let mut proposal_senders: Vec<Vec<u8>> = Vec::with_capacity(proposals.len());
            for (i, proposal_bytes) in proposals.iter().enumerate() {
                let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(proposal_bytes)?;
                let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;
                let processed = group.process_message(&provider, protocol_message)?;
                let sender = processed.credential().serialized_content().to_vec();
                match processed.into_content() {
                    ProcessedMessageContent::ProposalMessage(proposal) => {
                        group.store_pending_proposal(
                            provider.storage(),
                            proposal.as_ref().clone(),
                        )?;
                        proposal_senders.push(sender);
                    }
                    _ => {
                        tracing::debug!(
                            group = group_id,
                            index = i,
                            "apply_remote_candidate: non-proposal in proposal slot",
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
                    "apply_remote_candidate: ignoring commit for wrong group ID (expected {group_id})"
                );
                return Ok(StagedCandidateResult::Aborted);
            }
            if protocol_message.epoch() < group.epoch() {
                tracing::debug!(
                    "apply_remote_candidate: ignoring stale commit from epoch {} (current: {})",
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
                        actions.push(MlsProposalAction::Add(id));
                    }
                    for remove in staged.remove_proposals() {
                        let removed_index = remove.remove_proposal().removed();
                        let id = group
                            .member(removed_index)
                            .map(|c| c.serialized_content().to_vec())
                            .ok_or(MlsError::UnknownLeafIndex(removed_index.u32()))?;
                        actions.push(MlsProposalAction::Remove(id));
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
                        "apply_remote_candidate: ignoring non-commit message for group {group_id}"
                    );
                    None
                }
            }
        };

        match outcome {
            Some((commit_sender, proposal_senders, self_removed, actions, staged)) => {
                self.pending_staged_commits
                    .write()?
                    .insert(group_id.to_string(), staged);
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

    fn merge_staged_commit(&self, group_id: &str) -> Result<(), MlsError> {
        let provider = self.make_provider();

        let staged = self
            .pending_staged_commits
            .write()?
            .remove(group_id)
            .ok_or_else(|| MlsError::NoPendingStagedCommit(group_id.to_string()))?;

        let mut groups = self.groups.write()?;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

        group.merge_staged_commit(&provider, staged)?;
        Ok(())
    }

    fn discard_staged_commit(&self, group_id: &str) -> Result<(), MlsError> {
        self.pending_staged_commits.write()?.remove(group_id);

        let provider = self.make_provider();
        let mut groups = self.groups.write()?;
        if let Some(group) = groups.get_mut(group_id) {
            let _ = group.clear_pending_proposals(provider.storage());
        }

        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Application messages
    // ══════════════════════════════════════════════════════════

    fn encrypt(&self, group_id: &str, plaintext: &[u8]) -> Result<Vec<u8>, MlsError> {
        let provider = self.make_provider();
        let signer = self.identity.signer();

        let mut groups = self.groups.write()?;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

        let message = group.create_message(&provider, signer, plaintext)?;
        Ok(message.to_bytes()?)
    }

    fn decrypt_application_only(
        &self,
        group_id: &str,
        ciphertext: &[u8],
    ) -> Result<DecryptResult, MlsError> {
        let provider = self.make_provider();

        let mut groups = self.groups.write()?;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

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

    fn decrypt(&self, group_id: &str, ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
        let provider = self.make_provider();

        let mut groups = self.groups.write()?;
        let group = groups
            .get_mut(group_id)
            .ok_or_else(|| MlsError::GroupNotFound(group_id.to_string()))?;

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
                "Ignoring commit on decrypt() path for group {}: use apply_remote_candidate() instead",
                group_id,
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
                    OpenMlsService::<S, I>::extract_proposal_action(group, proposal.proposal())?;

                group.store_pending_proposal(provider.storage(), proposal.as_ref().clone())?;
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
