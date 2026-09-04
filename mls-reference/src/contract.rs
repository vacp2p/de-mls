//! The contract: the value types that cross the seam and the [`GroupOps`]
//! trait an application's MLS service implements.
//!
//! Every method below is a MUST unless its documentation says otherwise. The
//! rationale for each is in `README.md`, kept there because an implementer
//! will reorganise this code and should still be able to read the reasons.

/// A message that came off the wire sealed and opened cleanly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    /// The sender's member id — its leaf signature key — taken from the MLS
    /// framing. Never from the payload: the payload is not authenticated.
    pub sender: Vec<u8>,
    /// The epoch the message was sealed at, which may be behind the current one.
    pub epoch: u64,
    /// The decrypted payload, handed on without interpretation.
    pub plaintext: Vec<u8>,
}

/// A membership change a commit carries. The order of a slice of these is the
/// order the proposals take inside the commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Seat a joiner. `member` is the id the joiner will have once seated —
    /// the signature key its `key_package` carries.
    Add {
        member: Vec<u8>,
        key_package: Vec<u8>,
    },
    /// Unseat a current member by its id.
    Remove { member: Vec<u8> },
}

/// What staging a peer's commit revealed about it, without applying it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedFacts {
    /// The committer, authenticated by MLS — not the id the wire envelope
    /// claims.
    pub sender: Vec<u8>,
    /// The epoch the commit was built at.
    pub epoch: u64,
    /// The adds and removes it performs, in commit order.
    pub actions: Vec<Action>,
    /// Every proposal it carries, adds and removes included. Above
    /// `actions.len()` it carries kinds the engine does not know, and the
    /// round rejects it.
    pub proposal_count: u32,
    /// Whether it removes us.
    pub self_removed: bool,
}

/// A commit this node built. It is pending: the epoch has not moved and the
/// commit may still lose its round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    /// [`GroupOps::commit_hash`] of `commit`.
    pub hash: [u8; 32],
    /// The actions actually committed, in commit order: what the router
    /// reports to the engine as this commit's facts. Shorter than the
    /// actions asked for when removes of non-members were skipped.
    pub actions: Vec<Action>,
    /// Proposals the commit carries; equals `actions.len()`.
    pub proposal_count: u32,
    /// The serialized commit, broadcast as is: a candidate on the wire is
    /// these bytes and nothing else.
    pub commit: Vec<u8>,
    /// The welcome for the members this commit adds, when it adds any. It is
    /// delivered after the commit merges and carries nothing else; the
    /// joiner's sync arrives as an ordinary control message.
    pub welcome: Option<Vec<u8>>,
}

/// The group after a merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub epoch: u64,
    /// Every current member id, at the new epoch.
    pub members: Vec<Vec<u8>>,
}

/// One group's MLS operations.
///
/// Construction is deliberately absent: creating, joining and loading a group
/// need the application's provider, signer, credential and configuration, and
/// no useful signature covers every implementation. The requirements still
/// hold, and are stated on [`Reference::create`](crate::Reference::create),
/// [`Reference::open_welcome`](crate::Reference::open_welcome) and
/// [`Reference::load`](crate::Reference::load):
///
/// - the group id MUST equal the conversation id bytes;
/// - opening a welcome MUST distinguish "not addressed to me" from a failure.
///
/// One live instance per storage scope: two instances over one store fork the
/// group state.
pub trait GroupOps {
    type Error: std::error::Error;

    /// The conversation this group belongs to.
    fn conversation_id(&self) -> &str;

    /// The current epoch, read from the group. Never cached: only a merge
    /// moves it, and only the caller knows when one happened.
    fn epoch(&self) -> u64;

    /// Our own member id.
    fn own_id(&self) -> Vec<u8>;

    /// Every current member id — the leaf signature keys, in leaf order.
    fn members(&self) -> Vec<Vec<u8>>;

    /// Encrypt `plaintext` for the group at the current epoch. The ratchet
    /// MUST be persisted before this returns: a message whose key the sender
    /// forgets is a message the group cannot account for.
    fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, Self::Error>;

    /// Decrypt a sealed frame, or drop it.
    ///
    /// `Ok(None)` is a drop, not a failure — the sender is at fault, or the
    /// message simply arrived outside the window this node keeps keys for.
    /// A frame MUST be dropped when it is for another group, when its epoch is
    /// newer than the current one, when it is older than
    /// `current - past_window`, or when it is older than the epoch we joined
    /// at. Commit and proposal content MUST NOT be processed here: commits
    /// enter through [`GroupOps::stage`] and nothing else generates proposals.
    ///
    /// The sender in [`Opened`] MUST come from the MLS signature.
    fn open(&mut self, ciphertext: &[u8]) -> Result<Option<Opened>, Self::Error>;

    /// The member id a key package will have once its holder is seated. Parse
    /// only — no validation, no group state — so it can be read from an
    /// announcement before anyone decides to admit the joiner.
    fn key_package_identity(bytes: &[u8]) -> Result<Vec<u8>, Self::Error>
    where
        Self: Sized;

    /// Full MLS validation of a key package. Called before proposing an add so
    /// a bad key package fails at the caller rather than at commit time.
    fn validate_key_package(&self, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Build a commit carrying `actions`.
    ///
    /// The proposals MUST ride inline in the commit, in the given order, and
    /// MUST NOT go through the proposal store. The commit MUST force a
    /// self-update, so every commit brings fresh entropy. Every key package
    /// MUST be validated. A remove of somebody who is not a member MUST be
    /// skipped rather than failing the commit; the skip shows in
    /// [`Built::proposal_count`].
    ///
    /// The commit stays pending: the epoch does not move and
    /// [`GroupOps::pending_hash`] starts returning its hash. Exactly one
    /// commit can be pending.
    fn build_commit(&mut self, actions: &[Action]) -> Result<Built, Self::Error>;

    /// The hash of our own pending commit, if we have one.
    fn pending_hash(&self) -> Option<[u8; 32]>;

    /// Stage a peer's commit: validate and decrypt it, learn what it does, and
    /// hold it — do not apply it.
    ///
    /// Several commits MUST be stageable at once, keyed by hash, and staging
    /// MUST NOT disturb our own pending commit; a commit round routinely has a
    /// node holding its own candidate and two rivals. A commit for another
    /// group or for a past epoch MUST be rejected.
    fn stage(&mut self, commit: &[u8]) -> Result<StagedFacts, Self::Error>;

    /// Apply a commit, advancing the epoch, and persist before returning.
    ///
    /// `hash` names either our own pending commit or one held by
    /// [`GroupOps::stage`]. Merging a peer's commit MUST clear our own pending
    /// commit first. Every commit still staged afterwards is stale and MUST be
    /// dropped.
    fn merge(&mut self, hash: [u8; 32]) -> Result<Applied, Self::Error>;

    /// Drop one staged commit. A hash that is not held is not an error.
    fn discard(&mut self, hash: [u8; 32]);

    /// Drop our own pending commit. Idempotent.
    fn clear_pending(&mut self) -> Result<(), Self::Error>;

    /// The name of a commit: SHA-256 over its serialized bytes. One function,
    /// so every node names a commit identically.
    fn commit_hash(commit: &[u8]) -> [u8; 32]
    where
        Self: Sized;

    /// Tear down all local state for this group. Idempotent, so a repeated
    /// teardown after leaving is safe.
    fn delete(&mut self) -> Result<(), Self::Error>;
}
