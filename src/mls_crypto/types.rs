//! MLS types and operation results.

/// A membership change the steward feeds into the commit pipeline.
#[derive(Clone, Debug)]
pub enum MlsCommitInput {
    /// Add a new member from their serialized key-package bytes.
    Add(Vec<u8>),
    /// Remove a member by their member-id.
    Remove(Vec<u8>),
}

/// A membership change read out of a single MLS proposal while staging a
/// commit's bundled proposals — the target's `member_id` for both Add and
/// Remove.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MlsProposalOutput {
    /// Add a member — member-id is read from the key package credential.
    Add(Vec<u8>),
    /// Remove a member — member-id of the removed member.
    Remove(Vec<u8>),
}

/// Coarse kind of an MLS wire message: the two control kinds the freeze
/// pipeline gates on, plus `Other` for everything else (application messages,
/// welcomes, unrecognized frames).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MlsMessageKind {
    Proposal,
    Commit,
    Other,
}

/// A decrypted inbound application message: the plaintext `payload` and the
/// `sender`'s MLS-authenticated leaf credential.
#[derive(Clone, Debug)]
pub struct DecryptedMessage {
    pub payload: Vec<u8>,
    pub sender: Vec<u8>,
}

/// Outcome of staging a remote commit candidate.
#[derive(Clone, Debug)]
pub enum StagedCandidateResult {
    /// Staged and ready to merge. `commit_sender` is the MLS-authenticated
    /// signer; staging checked that every bundled proposal came from it.
    Staged {
        commit_sender: Vec<u8>,
        /// Whether this commit removes us from the conversation.
        self_removed: bool,
        /// Membership changes (Add/Remove) carried by the commit's proposals.
        actions: Vec<MlsProposalOutput>,
    },
    /// Could not be staged — a stale (earlier-epoch) commit, a wrong group, or a
    /// slot holding the wrong message kind. Dropped without penalty: the commit
    /// never staged, so its sender isn't MLS-authenticated, and de-mls won't pin
    /// a violation on the forgeable wire `steward_member_id`. (The RFC counts an
    /// earlier-epoch commit as a steward violation, but only an authenticated
    /// committer can be scored for it.)
    Aborted,
    /// Not every bundled proposal was signed by `commit_sender`. MLS permits a
    /// commit to reference others' proposals; de-mls requires the committer to
    /// own each one, so this is a broken commit attributed to `commit_sender`.
    BundleSenderMismatch { commit_sender: Vec<u8> },
}

/// The raw MLS artifacts from building a commit candidate, before it's merged
/// or wrapped into the wire `CommitCandidate` envelope: the serialized
/// proposals, the commit, and a welcome when the batch adds members.
#[derive(Clone, Debug)]
pub struct CommitArtifacts {
    /// Serialized MLS proposal messages.
    pub proposals: Vec<Vec<u8>>,
    /// Serialized MLS commit message.
    pub commit: Vec<u8>,
    /// Welcome message for new members, when the batch has any adds.
    pub welcome: Option<Vec<u8>>,
}
