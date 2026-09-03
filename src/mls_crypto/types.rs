//! MLS types and operation results.

use openmls::prelude::Member;

use crate::mls_crypto::MemberId;

/// Describes a membership update resulting from a merge: the members added (with
/// their assigned leaves) and, as pre-merge [`MemberId`] handles, those removed.
#[derive(Clone, Debug, Default)]
pub struct MembershipDelta {
    pub added: Vec<Member>,
    pub removed: Vec<MemberId>,
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
    /// Add a joiner, identified by its signature key until it has a leaf index.
    Add(Vec<u8>),
    /// Remove a member by its `member_id` (leaf index).
    Remove(Vec<u8>),
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
    /// Staged and ready to merge; `commit_sender` is the MLS-authenticated
    /// signer.
    Staged {
        commit_sender: Vec<u8>,
        /// Whether this commit removes us from the conversation.
        self_removed: bool,
        /// Membership changes (Add/Remove) the commit performs.
        actions: Vec<MlsProposalOutput>,
        /// Total proposals of any type — checked against the wire claim.
        proposal_count: usize,
    },
    /// Could not be staged — a stale (earlier-epoch) commit, a wrong group, or
    /// bytes that aren't a commit at all. Dropped without penalty: the commit
    /// never staged, so its sender isn't MLS-authenticated, and de-mls won't pin
    /// a violation on the forgeable wire `steward_member_id`. (The RFC counts an
    /// earlier-epoch commit as a steward violation, but only an authenticated
    /// committer can be scored for it.)
    Aborted,
}

/// The raw MLS artifacts from building a commit candidate: the commit (its
/// proposals ride inline) and a welcome when the batch adds members.
#[derive(Clone, Debug)]
pub struct CommitArtifacts {
    /// Serialized MLS commit message, proposals inline.
    pub commit: Vec<u8>,
    /// Total proposals in the commit — the candidate's wire claim.
    pub proposal_count: usize,
    /// Welcome message for new members, when the batch has any adds.
    pub welcome: Option<Vec<u8>>,
}
