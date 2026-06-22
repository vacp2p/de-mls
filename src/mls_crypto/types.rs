//! MLS types and operation results.

use openmls::{key_packages::KeyPackageIn, prelude::DeserializeBytes};

use crate::mls_crypto::MlsError;

/// Serialized key package for joining a conversation. Carries the
/// TLS-serialized key package alongside the owner's member-id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPackageBytes {
    bytes: Vec<u8>,
    member_id: Vec<u8>,
}

impl KeyPackageBytes {
    pub fn new(bytes: Vec<u8>, member_id: Vec<u8>) -> Self {
        Self { bytes, member_id }
    }

    /// TLS-serialized key package bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Member-id of the key package's owner, extracted from the MLS
    /// credential at construction time.
    pub fn member_id(&self) -> &[u8] {
        &self.member_id
    }
}

/// Membership change as supplied to the steward's commit pipeline.
///
/// One of three "membership change" shapes used in the codebase. They
/// describe the same intent at different boundaries:
///
/// | Shape | Where | Carries |
/// |-------|-------|---------|
/// | [`crate::protos::de_mls::messages::v1::ConversationUpdateRequest`] | consensus wire | wire payload, also covers governance kinds (emergency / election) |
/// | [`MlsCommitInput`] | input to [`crate::mls_crypto::MlsService::create_commit_candidate`] | Add carries the full key package; Remove carries the target member-id |
/// | [`MlsProposalOutput`] | output of MLS staging / decryption | member-id-only for both Add and Remove |
#[derive(Clone, Debug)]
pub enum MlsCommitInput {
    /// Add a new member using their key package.
    Add(KeyPackageBytes),
    /// Remove a member by their member-id.
    Remove(Vec<u8>),
}

/// Membership change observed in a single MLS proposal — extracted from
/// incoming proposals (standalone or commit-bundled). Carries the
/// target's `member_id` bytes only; see [`MlsCommitInput`] for the
/// input shape and the boundary table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MlsProposalOutput {
    /// Add a member — member-id is read from the key package credential.
    Add(Vec<u8>),
    /// Remove a member — member-id of the removed member.
    Remove(Vec<u8>),
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
    /// Contains `(message_bytes, sender member_id)`.
    Application(Vec<u8>, Vec<u8>),
    /// We were removed from the conversation.
    /// Contains the authenticated sender member_id.
    Removed(Vec<u8>),
    /// Proposal stored (no action needed).
    /// Contains `(sender member_id, action)`.
    ProposalStored(Vec<u8>, MlsProposalOutput),
    /// Message ignored (wrong conversation/epoch).
    Ignored,
}

/// Outcome of staging a remote commit. `Staged` is the happy path;
/// `Aborted` is a benign rejection (cleanup MLS state, no penalty);
/// `BundleSenderMismatch` is a protocol violation (caller emits
/// `BROKEN_COMMIT` evidence against `commit_sender`).
#[derive(Clone, Debug)]
pub enum StagedCandidateResult {
    /// Candidate staged successfully. `commit_sender` is the
    /// MLS-authenticated signer of the commit; the de-mls invariant
    /// "all bundled proposals match the commit sender" was checked
    /// during staging.
    Staged {
        commit_sender: Vec<u8>,
        /// Whether this commit removes us from the conversation.
        self_removed: bool,
        /// Membership changes (Add/Remove) contained in the commit's proposals.
        actions: Vec<MlsProposalOutput>,
    },
    /// Candidate was benign but not processable (stale epoch, wrong
    /// group, non-proposal in proposal slot, non-commit in commit slot).
    /// Caller cleans MLS state and drops the candidate without penalty.
    Aborted,
    /// One or more bundled proposals weren't signed by the commit
    /// sender. MLS itself allows commits to reference others'
    /// proposals, but de-mls's protocol layer requires every bundled
    /// proposal to come from the committer. Caller emits a
    /// `BROKEN_COMMIT` violation against `commit_sender`.
    BundleSenderMismatch { commit_sender: Vec<u8> },
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

/// Parse TLS-serialized key package bytes and extract the leaf-credential
/// member_id. Returns `(key_package_bytes, member_id)` — the bytes are
/// passed through unchanged so the caller can re-broadcast them on the
/// wire without a second serialization pass.
pub fn key_package_bytes_from_tls(bytes: Vec<u8>) -> Result<(Vec<u8>, Vec<u8>), MlsError> {
    let (kp_in, _rest) =
        KeyPackageIn::tls_deserialize_bytes(&bytes).map_err(MlsError::KeyPackageTls)?;
    let member_id = kp_in
        .unverified_credential()
        .credential
        .serialized_content()
        .to_vec();
    Ok((bytes, member_id))
}
