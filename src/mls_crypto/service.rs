//! Main MLS service providing all cryptographic operations.

use std::collections::HashMap;
use std::sync::RwLock;

use alloy::primitives::Address;
use openmls::credentials::CredentialWithKey;
use openmls::group::{GroupId, MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig};
use openmls::prelude::{
    BasicCredential, Ciphersuite, DeserializeBytes, MlsMessageBodyIn, MlsMessageIn,
    ProcessedMessageContent, ProtocolMessage, StagedWelcome,
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::{MemoryStorage, RustCrypto};
use openmls_traits::OpenMlsProvider;

use crate::mls_crypto::{
    error::{IdentityError, MlsError, MlsServiceError, Result, StorageError},
    identity::IdentityData,
    storage::DeMlsStorage,
    types::{CommitResult, DecryptResult, GroupUpdate, KeyPackageBytes},
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

    /// Decrypt/process an inbound MLS message.
    ///
    /// Handles application messages, proposals, and commits.
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

        // Ignore messages from old epochs - they can't be processed after the group advances
        if protocol_message.epoch() < group.epoch() {
            tracing::debug!(
                "Ignoring message from old epoch {} (current: {})",
                protocol_message.epoch().as_u64(),
                group.epoch().as_u64()
            );
            return Ok(DecryptResult::Ignored);
        }

        let processed = group.process_message(&provider, protocol_message)?;

        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                Ok(DecryptResult::Application(app.into_bytes()))
            }
            ProcessedMessageContent::ProposalMessage(proposal) => {
                group.store_pending_proposal(provider.storage(), proposal.as_ref().clone())?;
                Ok(DecryptResult::ProposalStored)
            }
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                let removed = staged.self_removed();
                group.merge_staged_commit(&provider, *staged)?;
                if removed {
                    if group.is_active() {
                        return Err(MlsError::Service(MlsServiceError::GroupStillActive));
                    }
                    Ok(DecryptResult::Removed)
                } else {
                    Ok(DecryptResult::CommitProcessed)
                }
            }
            ProcessedMessageContent::ExternalJoinProposalMessage(_) => Ok(DecryptResult::Ignored),
        }
    }

    // ══════════════════════════════════════════════════════════
    // Steward
    // ══════════════════════════════════════════════════════════

    /// Create proposals for membership changes and commit them.
    ///
    /// This is the core steward operation: takes a list of add/remove
    /// operations, creates MLS proposals, and commits them in a batch.
    pub fn commit(&self, group_id: &str, updates: &[GroupUpdate]) -> Result<CommitResult> {
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
        group.merge_pending_commit(&provider)?;

        let welcome_bytes = match welcome {
            Some(w) => Some(w.to_bytes()?),
            None => None,
        };

        Ok(CommitResult {
            proposals: mls_proposals,
            commit: commit_msg.to_bytes()?,
            welcome: welcome_bytes,
        })
    }

    // ══════════════════════════════════════════════════════════
    // Internal
    // ══════════════════════════════════════════════════════════

    fn make_provider(&self) -> MlsProvider<'_> {
        MlsProvider {
            crypto: &self.crypto,
            storage: self.storage.mls_storage(),
        }
    }
}
