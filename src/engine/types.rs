//! The vocabulary that crosses the engine boundary: identities, the facts the
//! router hands in, and the [`Output`] every driving call returns.
//!
//! Nothing here names an MLS type. Identities are bytes the group issued,
//! wire payloads are protobuf bytes, and time is a [`Timestamp`] the caller
//! supplies.

use std::{fmt, time::Duration};

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

/// The name every side knows one commit by. The application chooses it,
/// one function of the commit on every node (a digest of the bytes, the
/// epoch authenticator it produces); the engine stores and compares it.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommitHash(Vec<u8>);

impl CommitHash {
    /// The name's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for CommitHash {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<&[u8]> for CommitHash {
    fn from(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
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
    /// Replace `member`'s leaf with the update it proposed.
    Update { member: MemberId },
}

/// The kind of change an [`Action`] or an approved request makes, without
/// its payload: what the engine compares a commit against the vote by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ActionKind {
    Add,
    Remove,
    Update,
}

impl Action {
    /// The kind of change this action makes.
    pub(crate) fn kind(&self) -> ActionKind {
        match self {
            Action::Add { .. } => ActionKind::Add,
            Action::Remove { .. } => ActionKind::Remove,
            Action::Update { .. } => ActionKind::Update,
        }
    }

    /// The member this action is about.
    pub fn member(&self) -> &MemberId {
        match self {
            Action::Add { member, .. } | Action::Remove { member } | Action::Update { member } => {
                member
            }
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
    /// The adds, removes and updates the commit carries, in commit order.
    pub actions: Vec<Action>,
    /// Whether the commit removes this member.
    pub self_removed: bool,
}

/// The lifecycle phase of a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Normal operation.
    Working,
    /// A round is open: proposals and votes continue, and the candidate is
    /// validated when the window ends.
    Freezing,
    /// A candidate has been picked and is being merged.
    Selection,
    /// No steward list covers the current epoch: a sync or an election is
    /// awaited; rounds and proposals are held.
    Syncing,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase::Working => write!(f, "Working"),
            Phase::Freezing => write!(f, "Freezing"),
            Phase::Selection => write!(f, "Selection"),
            Phase::Syncing => write!(f, "Syncing"),
        }
    }
}

/// How a consensus session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The group said YES. Membership work is queued for the next
    /// commit; an election or a deadlock skip takes effect at once.
    Approved,
    /// The group said NO. A decision: the same request is not filed
    /// again this epoch.
    Rejected,
    /// The timeout passed with neither side at threshold, silent peers
    /// counted. Not a decision: filing again is the router's call.
    Failed,
}

/// Why a candidate was discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateRejection {
    /// Its sender is not the epoch steward this node expects.
    NotEpochSteward,
    /// It carries an action this node has not approved.
    ActionsMismatch,
}

impl fmt::Display for CandidateRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandidateRejection::NotEpochSteward => write!(f, "not the epoch steward"),
            CandidateRejection::ActionsMismatch => write!(f, "carries an action not approved here"),
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
    /// automatically once `voting_delay` passes. For an invite the request
    /// carries the joiner's key package bytes, which the engine never
    /// parses; a caller that wants to check them does so before voting.
    VoteRequested {
        proposal_id: u32,
        request: ConversationUpdateRequest,
    },
    /// A consensus session ended; `verdict` says how. A vote, not a
    /// landing: watch [`Self::MembersChanged`] for what the commit applied.
    ConsensusReached {
        proposal_id: u32,
        verdict: Verdict,
        timestamp: u64,
    },
    /// `received` of the `expected` stewards have sent a candidate this
    /// round. Emitted when the count changes.
    CommitRoundProgress { received: usize, expected: usize },
    /// A `ConversationSync` was adopted and its steward list installed.
    SyncApplied,
    /// A round closed with no valid commit from the epoch steward. The
    /// batch stays queued and the next round opens on its own; call
    /// [`super::Engine::request_recovery`] to put skipping the steward to
    /// a vote. `steward` is `None` when no eligible steward is left.
    CommitMissing {
        epoch: u64,
        steward: Option<MemberId>,
    },
    /// A candidate from `sender` was rejected. The router cannot tell a
    /// misbehaving committer from its own stale picture; a `request_sync`
    /// settles it.
    CandidateRejected {
        sender: MemberId,
        reason: CandidateRejection,
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
    /// A merged commit replaced `member`'s leaf.
    MemberUpdated { member: MemberId },
    /// A peer proposed `update` for its own leaf; it is approved on
    /// arrival. Process the update on the group and hold it: a commit
    /// carries a peer's update by reference, so every member must hold it
    /// before that commit can be built or staged. The engine never parses
    /// the bytes.
    UpdateProposed { member: MemberId, update: Vec<u8> },
    /// The group refused these approved actions at the build (a key package
    /// that expired, an update it would not hold); the epoch steward dropped
    /// them and every member follows. They land in no commit: whoever still
    /// wants one files it again with fresh material.
    ActionsDropped { actions: Vec<Action> },
    /// The router merged a commit the engine did not decide: the group's
    /// facts were adopted without the vote's judgment, and the same
    /// `Output` asks the stewards for a sync.
    CommitAdopted { hash: CommitHash },
}

/// What one driving call left for the application: events to observe and
/// the next wakeup. What to build and what to merge are read through
/// [`super::Engine::commit_to_build`] and [`super::Engine::commit_to_apply`]
/// after the call. Messages for the group go out through
/// [`crate::traits::Publish`] during the call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Observe.
    pub events: Vec<Event>,
    /// Call [`super::Engine::tick`] no later than this.
    pub wakeup: Option<Duration>,
}

impl Output {
    /// True when nothing was observed. `wakeup` alone does not count: a
    /// timer is armed, not work.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Append `other`'s events after this one's and keep the earlier wakeup.
    pub fn merge(&mut self, other: Output) {
        self.events.extend(other.events);
        self.wakeup = match (self.wakeup, other.wakeup) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_merge_keeps_order_and_earliest_wakeup() {
        let mut a = Output {
            events: vec![Event::SyncApplied],
            wakeup: Some(Duration::from_secs(5)),
        };
        let b = Output {
            events: vec![Event::PhaseChange(Phase::Working)],
            wakeup: Some(Duration::from_secs(2)),
        };
        a.merge(b);
        assert_eq!(a.events.len(), 2);
        assert!(matches!(a.events[0], Event::SyncApplied));
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
}
