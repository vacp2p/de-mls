//! MLS types and operation results.

use openmls::key_packages::KeyPackage as MlsKeyPackage;

use super::error::{MlsError, MlsServiceError};

/// Serialized key package for joining groups.
///
/// Contains the TLS-serialized key package bytes and the owner's wallet identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPackageBytes {
    bytes: Vec<u8>,
    identity: Vec<u8>,
}

impl KeyPackageBytes {
    pub fn new(bytes: Vec<u8>, identity: Vec<u8>) -> Self {
        Self { bytes, identity }
    }

    /// Get the serialized key package bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Membership change for commit.
#[derive(Clone, Debug)]
pub enum GroupUpdate {
    /// Add a new member using their key package.
    Add(KeyPackageBytes),
    /// Remove a member by their wallet address (20 bytes).
    Remove(Vec<u8>),
}

/// Result of decrypting an inbound message.
#[derive(Clone, Debug)]
pub enum DecryptResult {
    /// Application message decrypted successfully.
    Application(Vec<u8>),
    /// We were removed from the group.
    Removed,
    /// Proposal stored (no action needed).
    ProposalStored,
    /// Commit processed, group updated.
    CommitProcessed,
    /// Message ignored (wrong group/epoch).
    Ignored,
}

/// Result of creating a commit.
#[derive(Clone, Debug)]
pub struct CommitResult {
    /// Serialized MLS proposal messages.
    pub proposals: Vec<Vec<u8>>,
    /// Serialized MLS commit message.
    pub commit: Vec<u8>,
    /// Optional welcome message for new members (if any adds).
    pub welcome: Option<Vec<u8>>,
}

/// Parse a JSON-serialized key package and extract the identity.
///
/// Returns `(key_package_bytes, identity)` where:
/// - `key_package_bytes` is the original JSON bytes (passed through)
/// - `identity` is the wallet address extracted from the credential
pub fn key_package_bytes_from_json(json_bytes: Vec<u8>) -> Result<(Vec<u8>, Vec<u8>), MlsError> {
    let kp: MlsKeyPackage =
        serde_json::from_slice(&json_bytes).map_err(MlsServiceError::InvalidKeyPackage)?;
    let identity = kp.leaf_node().credential().serialized_content().to_vec();
    Ok((json_bytes, identity))
}
