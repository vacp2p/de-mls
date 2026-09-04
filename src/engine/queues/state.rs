//! The `EngineQueues` struct, its settled-membership bookkeeping, the
//! committed-hash dedup window, and the early-candidate stash. The bounded
//! dedup set and the in-flight-proposal metadata type live here too, shared
//! with the sibling modules that extend `EngineQueues`.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use indexmap::{IndexMap, IndexSet};

use crate::{
    engine::{
        proposal_kind::ProposalKind,
        types::{CommitHash, MembershipDelta, StagedFacts},
        util::member_set,
    },
    protos::de_mls::messages::v1::{CommitCandidate, ConversationUpdateRequest},
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

/// Represents a pending membership update that may not yet be committed.
///
/// Buffered by each member. If the epoch steward doesn't commit the change,
/// the next steward can handle it. Removed once applied or after a set number
/// of epochs since first seen.
#[derive(Clone, Debug)]
pub struct PendingUpdate {
    pub request: ConversationUpdateRequest,
    /// Epoch at which this update was first observed locally.
    pub first_seen_epoch: u64,
}

/// A candidate that arrived and staged cleanly before the local node approved
/// the proposals it carries: the envelope as it came off the wire, the hash it
/// was admitted under, and the facts the router learned by staging it.
#[derive(Clone, Debug)]
pub struct EarlyCandidate {
    pub hash: CommitHash,
    pub envelope: CommitCandidate,
    pub facts: StagedFacts,
}

/// A capacity-bounded, insertion-ordered set: dedups by value and evicts the
/// oldest entry once full. Backs the commit / welcome dedup windows and the
/// consensus-outcome cache.
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
    /// treated identically. The record that a change is already being voted on,
    /// so the epoch steward doesn't re-propose it out of `pending_updates` (RFC
    /// §Consensus Types: the steward collects and commits YES-voted proposals;
    /// it does not re-create them). Holds only what the queues query; the full
    /// request lives in consensus storage.
    pub(crate) voting_proposals: HashMap<ProposalId, VotingMeta>,
    /// Active emergency criteria proposals not yet finalized by consensus.
    /// While non-empty, lower-priority proposals MUST be blocked (RFC §Partial Freeze).
    pub(crate) active_emergency_ids: HashSet<ProposalId>,
    /// Recent commit hashes for dedup.
    committed_batch_hashes: BoundedSet<CommitHash>,
    /// Buffer of membership updates (Add/Remove) that every member records so a
    /// future epoch steward can retry them if the current one fails to commit.
    pub(crate) pending_updates: HashMap<Vec<u8>, PendingUpdate>,
    /// Bounded FIFO of proposal IDs with a locally-observed consensus outcome.
    /// Used by the outcome handler to drop library re-emissions and by the
    /// vote-forwarding path to distinguish benign late peer votes (session was
    /// trimmed after resolution) from votes for unknown proposal IDs.
    resolved_proposals: BoundedSet<ProposalId>,
    /// When `Some(target)`, the next freeze cycle commits only the
    /// `RemoveMember(target)` entry; other approvals wait so they don't
    /// dilute the fast-removal intent.
    pub(crate) urgent_commit_target: Option<Vec<u8>>,
    /// Candidates that staged before the local node approved their proposals;
    /// replayed once approval lands so the proposer doesn't fall an epoch
    /// behind. Epoch-tagged; stale entries dropped on the next stash/take.
    early_candidates: Vec<(u64, EarlyCandidate)>,
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
    /// Membership change from the just-merged commit, stashed when the router
    /// reports the merge for the post-commit reconcile (scoring, join epochs,
    /// pending-update pruning) to consume.
    pending_membership_delta: Option<MembershipDelta>,
}

impl EngineQueues {
    pub fn new(dedup_window: usize) -> Self {
        Self {
            approved_proposals: IndexMap::new(),
            voting_proposals: HashMap::new(),
            active_emergency_ids: HashSet::new(),
            committed_batch_hashes: BoundedSet::new(dedup_window),
            pending_updates: HashMap::new(),
            resolved_proposals: BoundedSet::new(RESOLVED_PROPOSAL_CACHE_CAPACITY),
            urgent_commit_target: None,
            early_candidates: Vec::new(),
            member_join_epoch: HashMap::new(),
            skipped_stewards: HashSet::new(),
            pending_membership_delta: None,
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

    /// Store the membership change a merged commit made,
    /// to be processed by the post-commit reconcile step.
    pub fn store_membership_delta(&mut self, delta: MembershipDelta) {
        self.pending_membership_delta = Some(delta);
    }

    /// Take the stashed membership delta, or an empty one when no commit merged
    /// this cycle.
    pub fn take_membership_delta(&mut self) -> MembershipDelta {
        self.pending_membership_delta.take().unwrap_or_default()
    }

    /// Updates queue state from the saved membership delta:
    /// adds join epochs for new members, removes data for members who left,
    /// and clears resolved pending or approved entries.
    /// This happens during commit finalization, before steward-list reconciliation,
    /// so just-joined members are marked as unsettled for this epoch.
    pub fn apply_membership_delta_bookkeeping(&mut self, epoch: u64) {
        let Some(delta) = self.pending_membership_delta.clone() else {
            return;
        };
        for member in &delta.removed {
            self.pending_updates.remove(member);
            self.drop_approved_removals_for(member);
            // Drop the departed member so a later re-join records a fresh join
            // epoch rather than inheriting its earlier one.
            self.member_join_epoch.remove(member);
        }
        for member in &delta.added {
            // A buffered invite is keyed by the joiner's id, the same id the
            // seated member now has.
            self.pending_updates.remove(member);
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

    // ─────────────────────────── Committed-Hash Dedup ───────────────────────────

    /// Check if a commit hash has already been committed (in committed history).
    ///
    /// This is the committed-history window only; dedup of candidates still in
    /// the round is the commit-round buffer's own.
    pub(crate) fn has_committed_hash(&self, commit_hash: &CommitHash) -> bool {
        self.committed_batch_hashes.contains(commit_hash)
    }

    /// Record a committed batch's hash for future dedup.
    pub(crate) fn insert_committed_hash(&mut self, commit_hash: CommitHash) {
        self.committed_batch_hashes.insert(commit_hash);
    }

    // ──────────────────────── Early (pre-approval) Candidates ────────────────────────

    /// Stash a candidate that staged before its proposals were locally
    /// approved. Deduped by commit hash and capped at `max`. Entries tagged
    /// with a different epoch are dropped — the node has moved on.
    pub fn stash_early_candidate(&mut self, epoch: u64, candidate: EarlyCandidate, max: usize) {
        self.early_candidates.retain(|(e, _)| *e == epoch);
        let duplicate = self
            .early_candidates
            .iter()
            .any(|(_, c)| c.hash == candidate.hash);
        if duplicate || self.early_candidates.len() >= max {
            return;
        }
        self.early_candidates.push((epoch, candidate));
    }

    /// Remove and return stashed candidates for `epoch`, discarding any tagged
    /// with a different (stale) epoch. Clears the stash.
    pub fn take_early_candidates(&mut self, epoch: u64) -> Vec<EarlyCandidate> {
        std::mem::take(&mut self.early_candidates)
            .into_iter()
            .filter(|(e, _)| *e == epoch)
            .map(|(_, c)| c)
            .collect()
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
