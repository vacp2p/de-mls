//! MLS types and operation results.

use openmls::prelude::{LeafNodeIndex, Member};

/// Describes a membership update resulting from a merge:
/// lists members who were added (with their assigned leaves)
/// and the members who was removed.
#[derive(Clone, Debug, Default)]
pub struct MembershipDelta {
    pub added: Vec<Member>,
    pub removed: Vec<LeafNodeIndex>,
}

/// A membership change the steward feeds into the commit pipeline.
#[derive(Clone, Debug)]
pub enum MlsCommitInput {
    /// Add a new member from their serialized key-package bytes.
    Add(Vec<u8>),
    /// Remove a member by their member-id.
    Remove(Vec<u8>),
}

/// Represents a membership change extracted from an individual MLS proposal while
/// processing the set of proposals included in a commit. For Add, the joiner is
/// identified by their MLS credential, as a leaf index is not assigned until after
/// the commit merges; for Remove, the targeted member is referenced by their
/// `member_id` (leaf index).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MlsProposalOutput {
    /// Add a joiner, identified by its credential until it has a leaf index.
    Add(Vec<u8>),
    /// Remove a member by its `member_id` (leaf index).
    Remove(Vec<u8>),
}

/// The kind of an MLS wire message, as far as the commit-round pipeline cares.
/// `Proposal` and `Commit` are the control kinds it gates on; `Other` covers
/// everything else (application messages, welcomes, unrecognized frames).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MlsMessageKind {
    Proposal,
    Commit,
    Other,
}

/// Represents a decrypted application message received from the network,
/// containing the plaintext `payload` and the MLS-authenticated sender as an
/// OpenMLS [`Member`] (including leaf index, credential, and keys).
#[derive(Clone, Debug)]
pub struct DecryptedMessage {
    pub payload: Vec<u8>,
    pub member: Member,
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
