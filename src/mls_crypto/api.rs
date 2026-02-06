//! MLS types and service traits.
//!
//! This module defines the core types and traits for MLS operations.
//! Implement these traits to provide custom MLS backends.

use alloy::hex;
use openmls::{
    group::MlsGroup,
    prelude::{Ciphersuite, KeyPackage},
};

use crate::mls_crypto::{IdentityError, MlsServiceError};

/// Serialized MLS key package with associated identity.
///
/// A key package is required to add a member to an MLS group. It contains
/// the member's public keys and is signed by their identity credential.
///
/// # Fields
/// - `bytes` - JSON-serialized OpenMLS `KeyPackage`
/// - `identity` - Wallet address (20 bytes) extracted from the credential
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPackageBytes {
    bytes: Vec<u8>,
    identity: Vec<u8>,
}

impl KeyPackageBytes {
    pub fn new(bytes: Vec<u8>, identity: Vec<u8>) -> Self {
        Self { bytes, identity }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn identity_bytes(&self) -> &[u8] {
        &self.identity
    }

    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(&self.identity))
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

pub(crate) fn key_package_from_json(bytes: &[u8]) -> Result<KeyPackage, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Parse a JSON-serialized key package and extract its identity.
///
/// Returns `(key_package_bytes, identity_bytes)` tuple.
pub fn key_package_bytes_from_json(
    bytes: Vec<u8>,
) -> Result<(Vec<u8>, Vec<u8>), serde_json::Error> {
    let key_package: KeyPackage = serde_json::from_slice(&bytes)?;
    let identity = key_package
        .leaf_node()
        .credential()
        .serialized_content()
        .to_vec();
    Ok((bytes, identity))
}

/// Wrapper around OpenMLS group state.
///
/// This handle encapsulates an `MlsGroup` and provides controlled access
/// to group operations. The handle is stored in [`GroupHandle`](crate::core::GroupHandle)
/// and accessed through `Arc<Mutex<>>` for thread safety.
#[derive(Debug)]
pub struct MlsGroupHandle {
    pub(crate) group: MlsGroup,
}

impl MlsGroupHandle {
    pub(crate) fn new(group: MlsGroup) -> Self {
        Self { group }
    }
}

/// A membership change to apply to an MLS group.
///
/// Used by [`MlsGroupService::create_batch_proposals`] to specify
/// what changes to include in the next commit.
#[derive(Clone, Debug)]
pub enum MlsGroupUpdate {
    /// Add a new member using their key package.
    AddMember(KeyPackageBytes),
    /// Remove a member by their wallet address (20 bytes).
    RemoveMember(Vec<u8>),
}

/// Result of processing an inbound MLS message.
///
/// Returned by [`MlsGroupService::process_inbound`] after decrypting
/// and processing an MLS protocol message.
#[derive(Clone, Debug)]
pub enum MlsProcessResult {
    /// An application message was decrypted successfully.
    /// Contains the plaintext bytes (typically a protobuf `AppMessage`).
    Application(Vec<u8>),
    /// This user was removed from the group.
    /// The group is no longer active and should be cleaned up.
    LeaveGroup,
    /// No action needed (proposal stored, wrong epoch, etc.).
    Noop,
}

/// Result of creating a batch of proposals and committing them.
///
/// Returned by [`MlsGroupService::create_batch_proposals`] after
/// the steward creates proposals and commits them to the group.
#[derive(Clone, Debug)]
pub struct BatchProposalsResult {
    /// Serialized MLS proposal messages (one per add/remove).
    pub proposals: Vec<Vec<u8>>,
    /// Serialized MLS commit message.
    pub commit: Vec<u8>,
    /// Optional welcome message for new members (if any adds).
    pub welcome: Option<Vec<u8>>,
}

/// Identity management and key package generation.
///
/// This trait handles wallet-based identity and MLS key package creation.
/// Implement this to use a different signing scheme or key storage.
///
/// # Default Implementation
///
/// [`OpenMlsIdentityService`](crate::mls_crypto::OpenMlsIdentityService) provides
/// a complete implementation using OpenMLS with Ethereum wallet addresses.
pub trait IdentityService: Send + Sync {
    /// The MLS ciphersuite used for cryptographic operations.
    const CIPHERSUITE: Ciphersuite;

    /// Initialize identity from a wallet address.
    ///
    /// Creates MLS credentials and signing keys from the 20-byte wallet address.
    fn create_identity(&mut self, wallet: &[u8]) -> Result<(), IdentityError>;

    /// Generate a new key package for joining groups.
    ///
    /// Key packages are single-use and should be regenerated after each join.
    fn generate_key_package(&mut self) -> Result<KeyPackageBytes, IdentityError>;

    /// Get the wallet address as raw bytes (20 bytes).
    fn identity(&self) -> &[u8];

    /// Get the wallet address as a checksummed hex string (`0x...`).
    fn identity_string(&self) -> String;

    /// Check if a key package hash reference belongs to this identity.
    ///
    /// Used to detect welcome messages intended for this user.
    fn is_key_package_exists(&self, kp_hash_ref: &[u8]) -> bool;
}

/// MLS group operations (create, join, encrypt, decrypt, commit).
///
/// This trait provides all MLS protocol operations needed for group messaging.
/// Implement this to use a different MLS library or configuration.
///
/// # Default Implementation
///
/// [`OpenMlsIdentityService`](crate::mls_crypto::OpenMlsIdentityService) implements
/// both `IdentityService` and `MlsGroupService`.
pub trait MlsGroupService: Send + Sync {
    /// Create a new MLS group with this user as the only member.
    ///
    /// The group name becomes the MLS group ID. The creator typically
    /// becomes the steward responsible for committing membership changes.
    fn create_group(&self, group_name: &str) -> Result<MlsGroupHandle, MlsServiceError>;

    /// Join a group by processing a welcome message.
    ///
    /// Returns the group handle and the group ID (group name as bytes).
    fn join_group_from_invite(
        &self,
        invite_bytes: &[u8],
    ) -> Result<(MlsGroupHandle, Vec<u8>), MlsServiceError>;

    /// Extract key package hash references from a welcome message.
    ///
    /// Used to check if a welcome is intended for this user.
    fn invite_new_member_hash_refs(
        &self,
        invite_bytes: &[u8],
    ) -> Result<Vec<Vec<u8>>, MlsServiceError>;

    /// Get all current group members' wallet addresses.
    fn group_members(&self, group: &MlsGroupHandle) -> Result<Vec<Vec<u8>>, MlsServiceError>;

    /// Get the current MLS epoch number.
    fn group_epoch(&self, group: &MlsGroupHandle) -> Result<u64, MlsServiceError>;

    /// Process an inbound MLS message (application, proposal, or commit).
    ///
    /// Returns [`MlsProcessResult`] indicating what was received.
    fn process_inbound(
        &self,
        group: &mut MlsGroupHandle,
        msg_bytes: &[u8],
    ) -> Result<MlsProcessResult, MlsServiceError>;

    /// Encrypt an application message for the group.
    ///
    /// Returns MLS ciphertext that only group members can decrypt.
    fn build_message(
        &self,
        group: &mut MlsGroupHandle,
        app_bytes: &[u8],
    ) -> Result<Vec<u8>, MlsServiceError>;

    /// Create MLS proposals for membership changes and commit them.
    ///
    /// This is the core steward operation: takes a list of add/remove
    /// operations, creates MLS proposals, and commits them in a batch.
    ///
    /// Returns proposals (for broadcast), commit, and optional welcome.
    fn create_batch_proposals(
        &self,
        group: &mut MlsGroupHandle,
        updates: &[MlsGroupUpdate],
    ) -> Result<BatchProposalsResult, MlsServiceError>;
}
