//! The vocabulary that crosses the engine boundary: identities, the facts the
//! router hands in, and the [`Output`] every driving call returns.
//!
//! Nothing here names an MLS type. Identities are bytes the group issued,
//! wire payloads are protobuf bytes, and time is a [`Timestamp`] the caller
//! supplies.

use std::{fmt, time::Duration};

use sha2::{Digest, Sha256};

pub use crate::engine::timestamp::Timestamp;
use crate::protos::de_mls::messages::v1::ConversationUpdateRequest;

/// A member of the conversation: the signature key of its leaf.
///
/// MLS keeps signature keys unique within a group, every node sees the same
/// key for the same leaf, and the key survives a plain rekey. A joiner has
/// the same id before it is seated (read from its key package) and after,
/// so adds and removes share one key space.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemberId(Vec<u8>);

impl MemberId {
    /// The raw signature-key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for MemberId {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<&[u8]> for MemberId {
    fn from(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }
}

impl fmt::Debug for MemberId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let head: Vec<String> = self.0.iter().take(4).map(|b| format!("{b:02x}")).collect();
        write!(f, "MemberId({}…)", head.join(""))
    }
}

/// SHA-256 of the MLS commit bytes; the name every side uses for one
/// candidate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitHash([u8; 32]);

impl CommitHash {
    /// Hash the serialized commit.
    pub fn of(commit_bytes: &[u8]) -> Self {
        Self(Sha256::digest(commit_bytes).into())
    }

    /// The digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for CommitHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for CommitHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let head: Vec<String> = self.0.iter().take(4).map(|b| format!("{b:02x}")).collect();
        write!(f, "CommitHash({}…)", head.join(""))
    }
}

/// One membership change inside a commit, in commit order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Seat `member`, whose key package is `key_package`.
    Add {
        member: MemberId,
        key_package: Vec<u8>,
    },
    /// Unseat `member`.
    Remove { member: MemberId },
}

impl Action {
    /// The member this action is about.
    pub fn member(&self) -> &MemberId {
        match self {
            Action::Add { member, .. } | Action::Remove { member } => member,
        }
    }
}

/// The member-set change one merged commit made, as the engine derives it
/// from the member sets before and after.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MembershipDelta {
    pub added: Vec<Vec<u8>>,
    pub removed: Vec<Vec<u8>>,
}

impl MembershipDelta {
    /// Compare two member sets.
    pub fn between(before: &[Vec<u8>], after: &[Vec<u8>]) -> Self {
        Self {
            added: after
                .iter()
                .filter(|m| !before.contains(m))
                .cloned()
                .collect(),
            removed: before
                .iter()
                .filter(|m| !after.contains(m))
                .cloned()
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

/// What the router learned by staging a candidate's commit on the group.
/// Every field comes from the MLS layer, none from the wire envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedFacts {
    /// The member whose signature the commit carries.
    pub sender: MemberId,
    /// The epoch the commit was built for.
    pub epoch: u64,
    /// The proposals the commit carries inline, in commit order.
    pub actions: Vec<Action>,
    /// Every proposal the commit carries, adds and removes included. When
    /// this exceeds `actions.len()` the commit carries proposal kinds the
    /// engine has no vocabulary for, and the round rejects it.
    pub proposal_count: u32,
    /// Whether the commit removes this member.
    pub self_removed: bool,
}

/// Something the router must execute against the group, in the order
/// given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A peer proposed adding `member` with `key_package`. Validate the key
    /// package and check that its identity is `member`, then report
    /// [`super::Engine::key_package_checked`]. The vote waits for the answer.
    ValidateKeyPackage {
        proposal_id: u32,
        member: MemberId,
        key_package: Vec<u8>,
    },
    /// This member is the epoch steward for the round: build a commit
    /// carrying exactly `actions`, keep it pending, broadcast the commit
    /// bytes, then report it through
    /// [`super::Engine::handle_candidate`] with the facts the build
    /// returned.
    BuildCommit { actions: Vec<Action> },
    /// Merge the staged or own commit named by `hash`, then report
    /// [`super::Engine::commit_applied`]. A loser clears its own pending
    /// commit first.
    Merge { hash: CommitHash },
    /// Drop these staged commits.
    Discard { hashes: Vec<CommitHash> },
    /// This member has left: tear the group down and drop the engine.
    Leave,
}

/// Bytes the router puts on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    /// A `ControlMessage`; seal at the current epoch and broadcast. The only
    /// kind: a candidate is broadcast by the router as raw commit bytes, not
    /// through this variant.
    Control(Vec<u8>),
}

/// The lifecycle phase of a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Normal operation.
    Working,
    /// New proposals are no longer accepted; candidates are being collected.
    Freezing,
    /// A candidate has been picked and is being merged.
    Selection,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase::Working => write!(f, "Working"),
            Phase::Freezing => write!(f, "Freezing"),
            Phase::Selection => write!(f, "Selection"),
        }
    }
}

/// Something the engine observed or did. Informational: dropping one loses
/// nothing the engine needs, and each request-like variant names its
/// fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A step the engine ran on its own failed; `operation` says which. The
    /// engine carries on.
    Error { operation: String, message: String },
    /// The conversation moved to a new phase.
    PhaseChange(Phase),
    /// A peer's proposal needs this member's vote. Silence votes
    /// automatically once `voting_delay` passes.
    VoteRequested {
        proposal_id: u32,
        request: ConversationUpdateRequest,
    },
    /// A consensus session finished. A vote, not a landing: watch
    /// [`Self::MembersChanged`] for what the commit applied.
    ConsensusReached {
        proposal_id: u32,
        approved: bool,
        timestamp: u64,
    },
    /// `received` of the `expected` stewards have sent a candidate this
    /// round. Emitted when the count changes.
    CommitRoundProgress { received: usize, expected: usize },
    /// A `ConversationSync` was adopted and its steward list installed.
    SyncApplied,
    /// Sync requests have gone unanswered `unanswered_sync_rounds` times in
    /// a row. The engine keeps asking.
    SyncUnanswered,
    /// A round closed with no valid commit from the epoch steward. The
    /// batch stays queued and the next round opens on its own; call
    /// [`super::Engine::request_recovery`] to put skipping the steward to
    /// a vote. `steward` is `None` when no eligible steward is left.
    CommitMissing {
        epoch: u64,
        steward: Option<MemberId>,
    },
    /// The deadlock vote passed: `steward` is skipped for this epoch and
    /// the next eligible steward on the list builds the commit.
    StewardSkipped { epoch: u64, steward: MemberId },
    /// A peer's score moved from `previous` to `score`. A fact, not a
    /// verdict; acting on it is the router's policy.
    MemberScoreChanged {
        member: MemberId,
        previous: i64,
        score: i64,
    },
    /// A commit changed the member set.
    MembersChanged {
        added: Vec<MemberId>,
        removed: Vec<MemberId>,
    },
}

/// Everything one driving call produced. The router executes it completely,
/// in order: `outbound`, then `decisions`, then `events`; see the router
/// contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Execute against the group, in order.
    pub decisions: Vec<Decision>,
    /// Seal and send.
    pub outbound: Vec<Outbound>,
    /// Observe.
    pub events: Vec<Event>,
    /// Call [`super::Engine::tick`] no later than this.
    pub wakeup: Option<Duration>,
}

impl Output {
    /// True when nothing is left to execute. `wakeup` alone does not count:
    /// a timer is armed, not work.
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty() && self.outbound.is_empty() && self.events.is_empty()
    }

    /// Append `other`'s work after this one's and keep the earlier wakeup.
    pub fn merge(&mut self, other: Output) {
        self.decisions.extend(other.decisions);
        self.outbound.extend(other.outbound);
        self.events.extend(other.events);
        self.wakeup = match (self.wakeup, other.wakeup) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
}

/// Why the router could not execute a [`Decision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionFailure {
    /// The decision that failed.
    pub decision: Decision,
    /// The group's error, for the log.
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_merge_keeps_order_and_earliest_wakeup() {
        let mut a = Output {
            decisions: vec![Decision::Leave],
            wakeup: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        let b = Output {
            decisions: vec![Decision::Discard { hashes: vec![] }],
            wakeup: Some(Duration::from_secs(2)),
            ..Default::default()
        };
        a.merge(b);
        assert_eq!(a.decisions.len(), 2);
        assert!(matches!(a.decisions[0], Decision::Leave));
        assert_eq!(a.wakeup, Some(Duration::from_secs(2)));
    }

    #[test]
    fn empty_means_no_work_even_with_a_wakeup() {
        let out = Output {
            wakeup: Some(Duration::from_secs(1)),
            ..Default::default()
        };
        assert!(out.is_empty());
    }

    #[test]
    fn commit_hash_is_sha256_of_the_bytes() {
        let h = CommitHash::of(b"commit");
        assert_eq!(h.as_bytes(), &<[u8; 32]>::from(Sha256::digest(b"commit")));
    }
}
