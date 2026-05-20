//! MLS types and operation results.

use openmls::key_packages::KeyPackage as MlsKeyPackage;

use crate::mls_crypto::MlsError;

/// Serialized key package for joining a conversation.
///
/// Carries the TLS-serialized key package and the owner's identity bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPackageBytes {
    bytes: Vec<u8>,
    identity: Vec<u8>,
}

impl KeyPackageBytes {
    pub fn new(bytes: Vec<u8>, identity: Vec<u8>) -> Self {
        Self { bytes, identity }
    }

    /// Serialized key package bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Identity bytes of the key package's owner, extracted from the
    /// MLS credential at construction time.
    pub fn identity(&self) -> &[u8] {
        &self.identity
    }
}

/// Membership change as supplied to the steward's commit pipeline.
///
/// One of three "membership change" shapes used in the codebase. They
/// describe the same underlying intent at different boundaries:
///
/// | Shape | Where | Carries |
/// |-------|-------|---------|
/// | [`crate::protos::de_mls::messages::v1::ConversationUpdateRequest`] | consensus wire | wire payload, also covers governance kinds (emergency / election) |
/// | [`MlsCommitInput`] | input to [`super::MlsService::create_commit_candidate`] | Add carries the full key package; Remove carries the target identity bytes |
/// | [`MlsProposalOutput`] | output of MLS staging / decryption | identity-only, plus `Other` for proposal kinds we don't construct |
#[derive(Clone, Debug)]
pub enum MlsCommitInput {
    /// Add a new member using their key package.
    Add(KeyPackageBytes),
    /// Remove a member by their identity bytes.
    Remove(Vec<u8>),
}

/// Membership change as observed in a single MLS proposal — extracted by
/// the MLS service from incoming proposals (standalone or commit-bundled).
///
/// See [`MlsCommitInput`] for the corresponding input shape and the
/// boundary table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MlsProposalOutput {
    /// Add a member — identity is read from the key package credential.
    Add(Vec<u8>),
    /// Remove a member — identity of the removed member.
    Remove(Vec<u8>),
    /// Any other proposal type (update, reinit, etc.) — we do not
    /// construct these ourselves; receipt is logged for diagnostics.
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
    /// We were removed from the conversation.
    /// Contains the authenticated sender identity.
    Removed(Vec<u8>),
    /// Proposal stored (no action needed).
    /// Contains `(sender_identity, action)`.
    ProposalStored(Vec<u8>, MlsProposalOutput),
    /// Message ignored (wrong conversation/epoch).
    Ignored,
}

/// Result of staging a remote candidate (proposal batch + commit).
///
/// Returned by `MlsService::stage_remote_commit`. The `Staged` variant
/// carries the MLS-authenticated senders of every staged proposal and of
/// the commit, plus the membership-change actions extracted from the
/// commit. `Aborted` signals a benign rejection (stale epoch, wrong conversation,
/// non-proposal in a proposal slot, non-commit in the commit slot) where
/// no validated outcome can be produced — the caller must NOT treat it as
/// a violation but should clean MLS state via `discard_staged_commit`.
#[derive(Clone, Debug)]
pub enum StagedCandidateResult {
    /// Candidate staged successfully. All identities are MLS-authenticated.
    Staged {
        /// Identity bytes of the commit sender (MLS-authenticated).
        commit_sender: Vec<u8>,
        /// Per-proposal sender identity, in input order. Caller cross-checks
        /// these against the commit sender to detect bundles signed by
        /// non-committers.
        proposal_senders: Vec<Vec<u8>>,
        /// Whether this commit removes us from the conversation.
        self_removed: bool,
        /// Membership changes (Add/Remove) contained in the commit's proposals.
        actions: Vec<MlsProposalOutput>,
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
/// - `identity` is the identity bytes from the leaf credential
pub fn key_package_bytes_from_json(json_bytes: Vec<u8>) -> Result<(Vec<u8>, Vec<u8>), MlsError> {
    let kp: MlsKeyPackage =
        serde_json::from_slice(&json_bytes).map_err(MlsError::KeyPackageJson)?;
    let identity = kp.leaf_node().credential().serialized_content().to_vec();
    Ok((json_bytes, identity))
}
