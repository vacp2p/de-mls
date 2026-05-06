//! MLS types and operation results.

use openmls::key_packages::KeyPackage as MlsKeyPackage;

use crate::mls_crypto::MlsError;

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

/// What an MLS proposal actually does (extracted before storing).
///
/// Used by the caller to verify that MLS proposals match the voted
/// `GroupUpdateRequest` proposals (review issue #4: payload equivalence).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MlsProposalAction {
    /// Add a member — carries the identity from the key package credential.
    Add(Vec<u8>),
    /// Remove a member — carries the identity of the removed member.
    Remove(Vec<u8>),
    /// Any other proposal type (update, reinit, etc.) — unexpected.
    Other(String),
}

/// Coarse-grained kind of an MLS wire message.
///
/// Used for strict lane checks in the core batch pipeline before processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MlsMessageKind {
    Application,
    Proposal,
    Commit,
    Welcome,
    Other,
}

/// Result of decrypting an inbound message.
#[derive(Clone, Debug)]
pub enum DecryptResult {
    /// Application message decrypted successfully.
    /// Contains `(message_bytes, sender_identity)`.
    Application(Vec<u8>, Vec<u8>),
    /// We were removed from the group.
    /// Contains the authenticated sender identity.
    Removed(Vec<u8>),
    /// Proposal stored (no action needed).
    /// Contains `(sender_identity, action)`.
    ProposalStored(Vec<u8>, MlsProposalAction),
    /// Message ignored (wrong group/epoch).
    Ignored,
}

/// Result of staging a remote candidate (proposal batch + commit).
///
/// Returned by `MlsService::apply_remote_candidate`. The `Staged` variant
/// carries the MLS-authenticated senders of every staged proposal and of
/// the commit, plus the membership-change actions extracted from the
/// commit. `Aborted` signals a benign rejection (stale epoch, wrong group,
/// non-proposal in a proposal slot, non-commit in the commit slot) where
/// no validated outcome can be produced — the caller must NOT treat it as
/// a violation but should clean MLS state via `discard_staged_commit`.
#[derive(Clone, Debug)]
pub enum StagedCandidateResult {
    /// Candidate staged successfully. All identities are MLS-authenticated.
    Staged {
        /// Identity (wallet bytes) of the commit sender.
        commit_sender: Vec<u8>,
        /// Per-proposal sender identity, in input order. Caller cross-checks
        /// these against the commit sender to detect bundles signed by
        /// non-committers.
        proposal_senders: Vec<Vec<u8>>,
        /// Whether this commit removes us from the group.
        self_removed: bool,
        /// Membership changes (Add/Remove) contained in the commit's proposals.
        actions: Vec<MlsProposalAction>,
    },
    /// Candidate was benign but not processable. Caller cleans MLS state.
    Aborted,
}

/// Result of creating a commit candidate (not merged yet).
#[derive(Clone, Debug)]
pub struct CommitCandidate {
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
        serde_json::from_slice(&json_bytes).map_err(MlsError::KeyPackageJson)?;
    let identity = kp.leaf_node().credential().serialized_content().to_vec();
    Ok((json_bytes, identity))
}
