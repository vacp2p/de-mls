//! Main MLS service providing all cryptographic operations.

use std::collections::HashMap;
use std::sync::RwLock;

use alloy::primitives::Address;
use openmls::credentials::CredentialWithKey;
use openmls::group::StagedCommit;
use openmls::group::{GroupId, MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig};
use openmls::prelude::{
    BasicCredential, Ciphersuite, DeserializeBytes, MlsMessageBodyIn, MlsMessageIn,
    ProcessedMessageContent, Proposal, ProtocolMessage, StagedWelcome,
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::{MemoryStorage, RustCrypto};
use openmls_traits::OpenMlsProvider;

use crate::mls_crypto::{
    error::{IdentityError, MlsError, MlsServiceError, Result, StorageError},
    identity::IdentityData,
    storage::DeMlsStorage,
    types::{
        CommitCandidate, DecryptResult, GroupUpdate, KeyPackageBytes, MlsMessageKind,
        MlsProposalAction, StagedCommitResult,
    },
};

/// The MLS ciphersuite used for all operations.
pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Internal OpenMLS provider that wraps storage.
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

/// Main MLS service - unified API for all MLS operations.
///
/// Groups are managed internally by group ID string. The service handles:
/// - Identity initialization and management
/// - Key package generation
/// - Group creation and joining
/// - Message encryption and decryption
/// - Steward commit operations
pub struct MlsService<S: DeMlsStorage> {
    storage: S,
    crypto: RustCrypto,
    identity: RwLock<Option<IdentityData>>,
    groups: RwLock<HashMap<String, MlsGroup>>,
    pending_staged_commits: RwLock<HashMap<String, StagedCommit>>,
}

impl<S> MlsService<S>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    /// Create a new MLS service with the given storage backend.
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            crypto: RustCrypto::default(),
            identity: RwLock::new(None),
            groups: RwLock::new(HashMap::new()),
            pending_staged_commits: RwLock::new(HashMap::new()),
        }
    }

    // ══════════════════════════════════════════════════════════
    // Identity
    // ══════════════════════════════════════════════════════════

    /// Initialize identity from wallet address.
    ///
    /// Creates MLS credentials and signing keys from the wallet address.
    /// Call this once before using any other methods.
    pub fn init(&self, wallet: Address) -> Result<()> {
        {
            let guard = self
                .identity
                .read()
                .map_err(|e| StorageError::Lock(e.to_string()))?;
            if guard.is_some() {
                return Err(MlsError::Identity(IdentityError::AlreadyInitialized));
            }
        }

        let credential = BasicCredential::new(wallet.as_slice().to_vec());
        let signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm())?;

        // Store signer in OpenMLS storage
        signer.store(self.storage.mls_storage())?;

        let data = IdentityData {
            wallet,
            credential: CredentialWithKey {
                credential: credential.into(),
                signature_key: signer.to_public_vec().into(),
            },
            signer,
        };

        let mut guard = self
            .identity
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        *guard = Some(data);
        Ok(())
    }

    /// Get the wallet address as a checksummed hex string ("0x...").
    pub fn wallet_hex(&self) -> String {
        self.identity
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|id| id.wallet.to_checksum(None)))
            .unwrap_or_default()
    }

    /// Get the wallet address as raw bytes.
    pub fn wallet_bytes(&self) -> Vec<u8> {
        self.identity
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|id| id.wallet.as_slice().to_vec()))
            .unwrap_or_default()
    }

    // ══════════════════════════════════════════════════════════
    // Key Packages
    // ══════════════════════════════════════════════════════════

    /// Generate a key package for joining a group.
    ///
    /// Key packages are single-use and should be regenerated after each join.
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes> {
        let guard = self
            .identity
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let identity = guard
            .as_ref()
            .ok_or(MlsError::Identity(IdentityError::IdentityNotFound))?;

        let provider = self.make_provider();

        let kp_bundle = openmls::key_packages::KeyPackage::builder().build(
            CIPHERSUITE,
            &provider,
            &identity.signer,
            identity.credential.clone(),
        )?;

        let kp = kp_bundle.key_package();
        let hash_ref = kp.hash_ref(provider.crypto())?.as_slice().to_vec();
        let bytes = serde_json::to_vec(kp).map_err(IdentityError::InvalidJson)?;

        self.storage.store_key_package_ref(&hash_ref)?;

        Ok(KeyPackageBytes::new(
            bytes,
            identity.wallet.as_slice().to_vec(),
        ))
    }

    // ══════════════════════════════════════════════════════════
    // Groups
    // ══════════════════════════════════════════════════════════

    /// Create a new MLS group.
    ///
    /// The group name becomes the MLS group ID. The creator becomes
    /// the only member and is typically the steward.
    pub fn create_group(&self, group_id: &str) -> Result<()> {
        let guard = self
            .identity
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let identity = guard
            .as_ref()
            .ok_or(MlsError::Identity(IdentityError::IdentityNotFound))?;

        let provider = self.make_provider();

        let config = MlsGroupCreateConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let group = MlsGroup::new_with_group_id(
            &provider,
            &identity.signer,
            &config,
            GroupId::from_slice(group_id.as_bytes()),
            identity.credential.clone(),
        )?;

        self.groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .insert(group_id.to_string(), group);

        Ok(())
    }

    /// Join a group from a welcome message.
    ///
    /// Returns the group ID on success. The welcome must be for us
    /// (contain one of our key package references).
    pub fn join_group(&self, welcome_bytes: &[u8]) -> Result<String> {
        let provider = self.make_provider();

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Err(MlsError::Service(MlsServiceError::UnexpectedMessageType)),
        };

        // Check if this welcome is for us
        let is_for_us = welcome.secrets().iter().any(|s| {
            self.storage
                .is_our_key_package(s.new_member().as_slice())
                .unwrap_or(false)
        });
        if !is_for_us {
            return Err(MlsError::Service(MlsServiceError::WelcomeNotForUs));
        }

        // Remove used key package references
        for secret in welcome.secrets() {
            let _ = self
                .storage
                .remove_key_package_ref(secret.new_member().as_slice());
        }

        let config = MlsGroupJoinConfig::builder().build();
        let group = StagedWelcome::new_from_welcome(&provider, &config, welcome, None)?
            .into_group(&provider)?;

        let group_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();

        self.groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .insert(group_id.clone(), group);

        Ok(group_id)
    }

    /// Check if a welcome message is for us (without joining).
    ///
    /// Returns true if the welcome contains one of our key package references.
    pub fn is_welcome_for_us(&self, welcome_bytes: &[u8]) -> Result<bool> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Ok(false),
        };

        Ok(welcome.secrets().iter().any(|s| {
            self.storage
                .is_our_key_package(s.new_member().as_slice())
                .unwrap_or(false)
        }))
    }

    /// Get all current group members as wallet addresses.
    pub fn members(&self, group_id: &str) -> Result<Vec<Vec<u8>>> {
        let groups = self
            .groups
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        Ok(group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
            .collect())
    }

    // ══════════════════════════════════════════════════════════
    // Messages
    // ══════════════════════════════════════════════════════════

    /// Encrypt an application message for the group.
    ///
    /// Returns MLS ciphertext that only group members can decrypt.
    pub fn encrypt(&self, group_id: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        let id_guard = self
            .identity
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let identity = id_guard
            .as_ref()
            .ok_or(MlsError::Identity(IdentityError::IdentityNotFound))?;

        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let message = group.create_message(&provider, &identity.signer, plaintext)?;
        Ok(message.to_bytes()?)
    }

    /// Decrypt an inbound MLS message, accepting only application messages.
    ///
    /// Unlike [`decrypt`], this method does NOT store proposals in the MLS
    /// pending queue. If a proposal or commit is encountered on the app
    /// subtopic, it is ignored — preventing MLS state pollution from rogue
    /// messages on the wrong channel.
    ///
    /// Use this for normal app messages on APP_MSG_SUBTOPIC.
    pub fn decrypt_application_only(
        &self,
        group_id: &str,
        ciphertext: &[u8],
    ) -> Result<DecryptResult> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            return Ok(DecryptResult::Ignored);
        }

        // Ignore messages from any non-current epoch — OpenMLS rejects both
        // old and future epochs, so gracefully ignore both to avoid hard errors
        // (e.g. when a joiner sends at epoch N+1 before we've merged our commit).
        if protocol_message.epoch() != group.epoch() {
            return Ok(DecryptResult::Ignored);
        }

        // Reject commits and proposals before process_message to avoid MLS
        // errors (e.g. MissingProposal when commit's proposals aren't stored).
        match protocol_message.content_type() {
            openmls::prelude::ContentType::Commit | openmls::prelude::ContentType::Proposal => {
                return Ok(DecryptResult::Ignored);
            }
            openmls::prelude::ContentType::Application => {}
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

    /// Process an MLS proposal from a commit candidate and store it in the
    /// MLS group's pending queue.
    ///
    /// This is used exclusively in the candidate pipeline during
    /// `finalize_freeze_round` Phase 3. Each MLS proposal from the chosen
    /// candidate is processed here so that the subsequent `process_commit()`
    /// can reference them.
    ///
    /// Returns `DecryptResult::ProposalStored` with the MLS-authenticated
    /// sender identity and the proposal action.
    pub fn process_candidate_proposal(
        &self,
        group_id: &str,
        proposal_bytes: &[u8],
    ) -> Result<DecryptResult> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(proposal_bytes)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        let processed = group.process_message(&provider, protocol_message)?;
        let sender_identity = processed.credential().serialized_content().to_vec();

        match processed.into_content() {
            ProcessedMessageContent::ProposalMessage(proposal) => {
                let action = Self::extract_proposal_action(group, proposal.proposal());

                // Store in MLS pending queue — required for commit processing
                group.store_pending_proposal(provider.storage(), proposal.as_ref().clone())?;
                Ok(DecryptResult::ProposalStored(sender_identity, action))
            }
            _ => Err(MlsError::Service(MlsServiceError::UnexpectedMessageType)),
        }
    }

    /// Decrypt/process an inbound MLS message.
    ///
    /// Handles application messages and proposals.
    ///
    /// Commit messages are intentionally rejected in this path and must be
    /// processed via [`process_commit`] + explicit merge/discard.
    pub fn decrypt(&self, group_id: &str, ciphertext: &[u8]) -> Result<DecryptResult> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
        let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

        // Check group ID
        if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
            return Ok(DecryptResult::Ignored);
        }

        // Ignore messages from any non-current epoch. Old epochs can't be
        // processed, and future epochs arrive when a joiner sends at epoch N+1
        // before we've merged our pending commit.
        if protocol_message.epoch() != group.epoch() {
            tracing::debug!(
                "Ignoring message from epoch {} (current: {})",
                protocol_message.epoch().as_u64(),
                group.epoch().as_u64()
            );
            return Ok(DecryptResult::Ignored);
        }

        if protocol_message.content_type() == openmls::prelude::ContentType::Commit {
            tracing::debug!(
                "Ignoring commit on decrypt() path for group {}: use process_commit() instead",
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
                // Extract the action before storing (consuming) the proposal.
                let action = Self::extract_proposal_action(group, proposal.proposal());

                group.store_pending_proposal(provider.storage(), proposal.as_ref().clone())?;
                Ok(DecryptResult::ProposalStored(sender_identity, action))
            }
            ProcessedMessageContent::StagedCommitMessage(_) => Ok(DecryptResult::Ignored),
            ProcessedMessageContent::ExternalJoinProposalMessage(_) => Ok(DecryptResult::Ignored),
        }
    }

    /// Inspect the untrusted outer kind of an MLS message.
    ///
    /// This is useful for strict lane checks (proposal lane vs commit lane)
    /// before deeper processing.
    pub fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind> {
        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(message_bytes)?;
        let protocol = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(_) => return Ok(MlsMessageKind::Welcome),
            MlsMessageBodyIn::PrivateMessage(m) => ProtocolMessage::PrivateMessage(m),
            MlsMessageBodyIn::PublicMessage(m) => ProtocolMessage::PublicMessage(Box::new(m)),
            _ => return Ok(MlsMessageKind::Other),
        };

        let kind = match protocol.content_type() {
            openmls::prelude::ContentType::Application => MlsMessageKind::Application,
            openmls::prelude::ContentType::Proposal => MlsMessageKind::Proposal,
            openmls::prelude::ContentType::Commit => MlsMessageKind::Commit,
        };
        Ok(kind)
    }

    // ══════════════════════════════════════════════════════════
    // Steward
    // ══════════════════════════════════════════════════════════

    /// Create proposals for membership changes and stage a commit candidate.
    ///
    /// This is the core steward operation: takes a list of add/remove
    /// operations, creates MLS proposals, and produces a commit candidate.
    ///
    /// This does NOT merge the pending commit.
    pub fn create_commit_candidate(
        &self,
        group_id: &str,
        updates: &[GroupUpdate],
    ) -> Result<CommitCandidate> {
        let id_guard = self
            .identity
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let identity = id_guard
            .as_ref()
            .ok_or(MlsError::Identity(IdentityError::IdentityNotFound))?;

        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let mut mls_proposals = Vec::new();

        for update in updates {
            match update {
                GroupUpdate::Add(key_package) => {
                    let kp: openmls::key_packages::KeyPackage =
                        serde_json::from_slice(key_package.as_bytes())
                            .map_err(MlsServiceError::InvalidKeyPackage)?;
                    let (mls_message_out, _proposal_ref) =
                        group.propose_add_member(&provider, &identity.signer, &kp)?;
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
                            group.propose_remove_member(&provider, &identity.signer, index)?;
                        mls_proposals.push(mls_message_out.to_bytes()?);
                    }
                }
            }
        }

        let (commit_msg, welcome, _group_info) =
            group.commit_to_pending_proposals(&provider, &identity.signer)?;

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

    /// Merge this member's own pending commit candidate.
    pub fn merge_own_commit(&self, group_id: &str) -> Result<()> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        group.merge_pending_commit(&provider)?;
        Ok(())
    }

    /// Discard this member's own pending commit candidate.
    pub fn discard_own_commit(&self, group_id: &str) -> Result<()> {
        let provider = self.make_provider();

        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        let _ = group.clear_pending_commit(provider.storage());
        let _ = group.clear_pending_proposals(provider.storage());
        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Split Commit Processing (inspect → validate → merge/discard)
    // ══════════════════════════════════════════════════════════

    /// Process an inbound commit without merging it.
    ///
    /// Calls `process_message()` to authenticate the commit sender and extract
    /// the membership changes, then stores the `StagedCommit` internally.
    /// The caller should validate the batch and then call either
    /// [`merge_staged_commit`] or [`discard_staged_commit`].
    pub fn process_commit(&self, group_id: &str, ciphertext: &[u8]) -> Result<StagedCommitResult> {
        let provider = self.make_provider();

        // Phase 1: Hold only the groups lock. Do all group work (deserialize,
        // authenticate, extract actions) and produce an owned StagedCommit.
        let outcome = {
            let mut groups = self
                .groups
                .write()
                .map_err(|e| StorageError::Lock(e.to_string()))?;
            let group = groups.get_mut(group_id).ok_or_else(|| {
                MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
            })?;

            let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(ciphertext)?;
            let protocol_message: ProtocolMessage = mls_message.try_into_protocol_message()?;

            if protocol_message.group_id().as_slice() != group.group_id().as_slice() {
                tracing::debug!(
                    "process_commit: ignoring commit for wrong group ID (expected {})",
                    group_id,
                );
                return Ok(StagedCommitResult::Ignored);
            }

            if protocol_message.epoch() < group.epoch() {
                tracing::debug!(
                    "process_commit: ignoring stale commit from epoch {} (current: {})",
                    protocol_message.epoch().as_u64(),
                    group.epoch().as_u64(),
                );
                return Ok(StagedCommitResult::Ignored);
            }

            let processed = group.process_message(&provider, protocol_message)?;
            let sender_identity = processed.credential().serialized_content().to_vec();

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
                            .unwrap_or_default();
                        actions.push(MlsProposalAction::Remove(id));
                    }

                    Some((sender_identity, self_removed, actions, *staged))
                }
                _ => {
                    tracing::debug!(
                        "process_commit: ignoring non-commit message for group {}",
                        group_id,
                    );
                    None
                }
            }
        }; // groups lock released

        // Phase 2: Store the staged commit under its own lock.
        match outcome {
            Some((sender_identity, self_removed, actions, staged)) => {
                self.pending_staged_commits
                    .write()
                    .map_err(|e| StorageError::Lock(e.to_string()))?
                    .insert(group_id.to_string(), staged);

                Ok(StagedCommitResult::Staged {
                    sender_identity,
                    self_removed,
                    actions,
                })
            }
            None => Ok(StagedCommitResult::Ignored),
        }
    }

    /// Merge a previously staged commit, advancing the MLS epoch.
    ///
    /// Call this after validation passes.
    pub fn merge_staged_commit(&self, group_id: &str) -> Result<()> {
        let provider = self.make_provider();

        // Phase 1: Take the staged commit out of the pending map.
        let staged = self
            .pending_staged_commits
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .remove(group_id)
            .ok_or_else(|| {
                MlsError::Service(MlsServiceError::NoPendingStagedCommit(group_id.to_string()))
            })?;
        // pending_staged_commits lock released — staged is an owned value.

        // Phase 2: Merge into the group under the groups lock.
        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let group = groups.get_mut(group_id).ok_or_else(|| {
            MlsError::Service(MlsServiceError::GroupNotFound(group_id.to_string()))
        })?;

        group.merge_staged_commit(&provider, staged)?;
        Ok(())
    }

    /// Discard a previously staged commit without merging.
    ///
    /// Call this when validation detects a violation or when a pre-authentication
    /// failure requires cleanup. Also clears pending proposals from MLS storage
    /// to keep the group state clean.
    pub fn discard_staged_commit(&self, group_id: &str) -> Result<()> {
        // Phase 1: Remove the staged commit (drop it).
        self.pending_staged_commits
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .remove(group_id);
        // pending_staged_commits lock released.

        // Phase 2: Clear pending proposals under the groups lock.
        let provider = self.make_provider();
        let mut groups = self
            .groups
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        if let Some(group) = groups.get_mut(group_id) {
            let _ = group.clear_pending_proposals(provider.storage());
        }

        Ok(())
    }

    // ══════════════════════════════════════════════════════════
    // Internal
    // ══════════════════════════════════════════════════════════

    fn extract_proposal_action(group: &MlsGroup, proposal: &Proposal) -> MlsProposalAction {
        match proposal {
            Proposal::Add(add) => {
                let id = add
                    .key_package()
                    .leaf_node()
                    .credential()
                    .serialized_content()
                    .to_vec();
                MlsProposalAction::Add(id)
            }
            Proposal::Remove(remove) => {
                let id = group
                    .member(remove.removed())
                    .map(|c| c.serialized_content().to_vec())
                    .unwrap_or_default();
                MlsProposalAction::Remove(id)
            }
            other => MlsProposalAction::Other(format!("{other:?}")),
        }
    }

    fn make_provider(&self) -> MlsProvider<'_> {
        MlsProvider {
            crypto: &self.crypto,
            storage: self.storage.mls_storage(),
        }
    }
}
