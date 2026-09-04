//! The `EngineQueues` struct and its settled-membership bookkeeping. The
//! bounded dedup set and the in-flight-proposal metadata type live here too,
//! shared with the sibling module that extends `EngineQueues`.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use indexmap::{IndexMap, IndexSet};

use crate::{
    engine::{proposal_kind::ProposalKind, types::MembershipDelta, util::member_set},
    protos::de_mls::messages::v1::ConversationUpdateRequest,
};

/// Consensus proposal identifier (assigned by the consensus service).
pub type ProposalId = u32;

/// What the queues need to reason about an in-flight proposal without holding
/// the full request — consensus storage keeps that. Just who it targets and
/// whether it is a steward election.
#[derive(Clone, Debug)]
pub(crate) struct VotingMeta {
    pub(crate) target: Option<Vec<u8>>,
    pub(crate) kind: ProposalKind,
}

/// A capacity-bounded, insertion-ordered set: dedups by value and evicts the
/// oldest entry once full. Backs the consensus-outcome cache.
#[derive(Clone, Debug)]
pub(crate) struct BoundedSet<T: Hash + Eq> {
    items: IndexSet<T>,
    capacity: usize,
}

impl<T: Hash + Eq> BoundedSet<T> {
    pub(crate) fn new(capacity: usize) -> Self {
        // Clamp to at least 1: capacity 0 would evict every insert, silently
        // disabling dedup.
        let capacity = capacity.max(1);
        Self {
            items: IndexSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Whether `item` is currently in the set.
    pub(crate) fn contains(&self, item: &T) -> bool {
        self.items.contains(item)
    }

    /// Record `item`; `true` if it was new (evicting the oldest once over
    /// capacity), `false` if already present.
    pub(crate) fn insert(&mut self, item: T) -> bool {
        if !self.items.insert(item) {
            return false;
        }
        while self.items.len() > self.capacity {
            self.items.shift_remove_index(0);
        }
        true
    }
}

/// Per-conversation protocol state. Stewards batch commits; members vote.
/// Construct with [`EngineQueues::new`].
pub struct EngineQueues {
    /// Proposals that passed consensus, waiting for steward to commit.
    /// `IndexMap` so insertion order (FIFO) is preserved without a side
    /// vector — library proposal IDs are content-derived hashes, so
    /// sort-by-id is not temporal.
    pub(crate) approved_proposals: IndexMap<ProposalId, ConversationUpdateRequest>,
    /// Index of proposals in flight through consensus — ours and peers',
    /// treated identically (RFC §Consensus Types: the steward collects and
    /// commits YES-voted proposals; it does not re-create them). Holds only
    /// what the queues query; the full request lives in consensus storage.
    pub(crate) voting_proposals: HashMap<ProposalId, VotingMeta>,
    /// Bounded FIFO of proposal IDs with a locally-observed consensus outcome.
    /// Used by the outcome handler to drop library re-emissions and by the
    /// vote-forwarding path to distinguish benign late peer votes (session was
    /// trimmed after resolution) from votes for unknown proposal IDs.
    resolved_proposals: BoundedSet<ProposalId>,
    /// When `Some(target)`, the next freeze cycle commits only the
    /// `RemoveMember(target)` entry; other approvals wait so they don't
    /// dilute the fast-removal intent.
    pub(crate) urgent_commit_target: Option<Vec<u8>>,
    /// Join epoch per member, keyed by member id, recorded from each merged
    /// commit's delta and pruned when a member departs (so a member that
    /// re-joins later records a fresh join epoch rather than inheriting its
    /// earlier one). Drives the "settled" check (see [`Self::is_settled`]):
    /// deterministic across nodes that apply the same commit, and answerable
    /// for any epoch — the `ConversationSync` a mid-election joiner adopts
    /// recomputes the unsettled set at a possibly past election epoch.
    pub(crate) member_join_epoch: HashMap<Vec<u8>, u64>,
    /// Stewards a deadlock vote skipped for the current epoch; cleared when
    /// a commit lands. Excluded from steward eligibility.
    skipped_stewards: HashSet<Vec<u8>>,
}

impl Default for EngineQueues {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineQueues {
    pub fn new() -> Self {
        Self {
            approved_proposals: IndexMap::new(),
            voting_proposals: HashMap::new(),
            resolved_proposals: BoundedSet::new(RESOLVED_PROPOSAL_CACHE_CAPACITY),
            urgent_commit_target: None,
            member_join_epoch: HashMap::new(),
            skipped_stewards: HashSet::new(),
        }
    }

    /// Build the eligibility predicate that the steward list's "live"
    /// position queries take. A candidate is eligible when they are a member
    /// of the conversation AND don't have a removal queued in
    /// `approved_proposals`. The closure borrows from `self` and `members`
    /// for the lifetime of the call.
    pub fn steward_eligibility(&self, members: &[Vec<u8>]) -> impl Fn(&[u8]) -> bool {
        let member_set = member_set(members);
        move |candidate: &[u8]| {
            !self.has_approved_removal(candidate)
                && !self.skipped_stewards.contains(candidate)
                && member_set.contains(candidate)
        }
    }

    /// A deadlock vote passed against `steward`: it is ineligible until a
    /// commit lands.
    pub fn skip_steward(&mut self, steward: Vec<u8>) {
        self.skipped_stewards.insert(steward);
    }

    /// Stewards skipped in the current epoch.
    #[cfg(test)]
    pub fn skipped_stewards(&self) -> &HashSet<Vec<u8>> {
        &self.skipped_stewards
    }

    /// A commit landed: every skip is spent.
    pub fn clear_skipped(&mut self) {
        self.skipped_stewards.clear();
    }

    // ─────────────────────────── Settled membership ───────────────────────────

    /// Updates queue state from the merged commit's delta: adds join epochs
    /// for new members and drops the departed member's approved-removal and
    /// join-epoch entries. This happens during commit finalization, before
    /// steward-list reconciliation, so just-joined members are marked as
    /// unsettled for this epoch.
    pub fn apply_membership_delta_bookkeeping(&mut self, epoch: u64, delta: &MembershipDelta) {
        for member in &delta.removed {
            self.drop_approved_removals_for(member);
            // Drop the departed member so a later re-join records a fresh join
            // epoch rather than inheriting its earlier one.
            self.member_join_epoch.remove(member);
        }
        for member in &delta.added {
            // Record this epoch as the member's join epoch — unsettled until the
            // next epoch, and answerable for any epoch a later sync recomputes.
            self.member_join_epoch.insert(member.clone(), epoch);
        }
    }

    /// Settled once the epoch has advanced past the member's join epoch — a
    /// just-joined member can't be a steward or voter until the next epoch. A
    /// member added in any prior epoch counts as settled, as does one this node
    /// never tracked (e.g. the creator).
    pub fn is_settled(&self, member_id: &[u8], current_epoch: u64) -> bool {
        self.member_join_epoch
            .get(member_id)
            .is_none_or(|&join_epoch| join_epoch < current_epoch)
    }

    /// The settled subset of `members` — steward-eligible this epoch.
    pub fn settled_members(&self, members: &[Vec<u8>], current_epoch: u64) -> Vec<Vec<u8>> {
        members
            .iter()
            .filter(|m| self.is_settled(m, current_epoch))
            .cloned()
            .collect()
    }

    // ─────────────────────────── Urgent (ECP-driven) Commit Target ───────────────────────────

    pub fn urgent_commit_target(&self) -> Option<&[u8]> {
        self.urgent_commit_target.as_deref()
    }

    /// Mark the next freeze cycle as urgent and committed-only-for `target`.
    pub(crate) fn set_urgent_commit_target(&mut self, target: Vec<u8>) {
        self.urgent_commit_target = Some(target);
    }

    pub(crate) fn take_urgent_commit_target(&mut self) -> Option<Vec<u8>> {
        self.urgent_commit_target.take()
    }

    // ─────────────────────────── Resolved-Outcome Cache ───────────────────────────

    pub(crate) fn is_consensus_outcome_applied(&self, proposal_id: ProposalId) -> bool {
        self.resolved_proposals.contains(&proposal_id)
    }

    pub(crate) fn mark_consensus_outcome_applied(&mut self, proposal_id: ProposalId) {
        self.resolved_proposals.insert(proposal_id);
    }
}

/// Capacity of the resolved-outcome dedup set (`resolved_proposals`): proposal
/// IDs with a locally observed consensus outcome, kept for the duplicate-drop
/// guard on consensus outcomes and the late-vote classifier on incoming votes.
/// Sized well above the consensus library's `max_sessions_per_scope` (default
/// 10) so a late vote arriving within any plausible peer-lag window still finds
/// its id cached.
const RESOLVED_PROPOSAL_CACHE_CAPACITY: usize = 256;
