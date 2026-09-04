//! Per-conversation protocol-queue state: approved/voting proposal queues,
//! early-candidate stash, pending-update buffer, urgent-commit target.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use indexmap::{IndexMap, IndexSet};

use crate::{
    engine::{
        proposal_kind::ProposalKind,
        types::{CommitHash, MembershipDelta, StagedFacts},
        util::{in_flight_target, member_set, self_leave_proposal_id, target_member_id_of},
    },
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, conversation_update_request,
    },
};

/// Consensus proposal identifier (assigned by the consensus service).
pub type ProposalId = u32;

/// What the queues need to reason about an in-flight proposal without holding
/// the full request — consensus storage keeps that. Just who it targets and
/// whether it is a steward election.
#[derive(Clone, Debug)]
struct VotingMeta {
    target: Option<Vec<u8>>,
    kind: ProposalKind,
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
struct BoundedSet<T: Hash + Eq> {
    items: IndexSet<T>,
    capacity: usize,
}

impl<T: Hash + Eq> BoundedSet<T> {
    fn new(capacity: usize) -> Self {
        // Clamp to at least 1: capacity 0 would evict every insert, silently
        // disabling dedup.
        let capacity = capacity.max(1);
        Self {
            items: IndexSet::with_capacity(capacity),
            capacity,
        }
    }

    /// Whether `item` is currently in the set.
    fn contains(&self, item: &T) -> bool {
        self.items.contains(item)
    }

    /// Record `item`; `true` if it was new (evicting the oldest once over
    /// capacity), `false` if already present.
    fn insert(&mut self, item: T) -> bool {
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
    approved_proposals: IndexMap<ProposalId, ConversationUpdateRequest>,
    /// Index of proposals in flight through consensus — ours and peers',
    /// treated identically. The record that a change is already being voted on,
    /// so the epoch steward doesn't re-propose it out of `pending_updates` (RFC
    /// §Consensus Types: the steward collects and commits YES-voted proposals;
    /// it does not re-create them). Holds only what the queues query; the full
    /// request lives in consensus storage.
    voting_proposals: HashMap<ProposalId, VotingMeta>,
    /// Active emergency criteria proposals not yet finalized by consensus.
    /// While non-empty, lower-priority proposals MUST be blocked (RFC §Partial Freeze).
    active_emergency_ids: HashSet<ProposalId>,
    /// Recent commit hashes for dedup.
    committed_batch_hashes: BoundedSet<CommitHash>,
    /// Buffer of membership updates (Add/Remove) that every member records so a
    /// future epoch steward can retry them if the current one fails to commit.
    pending_updates: HashMap<Vec<u8>, PendingUpdate>,
    /// Bounded FIFO of proposal IDs with a locally-observed consensus outcome.
    /// Used by the outcome handler to drop library re-emissions and by the
    /// vote-forwarding path to distinguish benign late peer votes (session was
    /// trimmed after resolution) from votes for unknown proposal IDs.
    resolved_proposals: BoundedSet<ProposalId>,
    /// When `Some(target)`, the next freeze cycle commits only the
    /// `RemoveMember(target)` entry; other approvals wait so they don't
    /// dilute the fast-removal intent.
    urgent_commit_target: Option<Vec<u8>>,
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
    member_join_epoch: HashMap<Vec<u8>, u64>,
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

    // ─────────────────────────── Approved Proposals ───────────────────────────

    pub fn approved_proposals_count(&self) -> usize {
        self.approved_proposals.len()
    }

    pub fn approved_proposals(&self) -> &IndexMap<ProposalId, ConversationUpdateRequest> {
        &self.approved_proposals
    }

    /// Member ids targeted by an in-flight proposal — either still voting or
    /// already approved. A buffered update whose target is in this set is
    /// already covered by a live proposal and must not be re-proposed.
    pub fn active_proposal_targets(&self) -> HashSet<Vec<u8>> {
        let voting = self
            .voting_proposals
            .values()
            .filter_map(|m| m.target.clone());
        let approved = self
            .approved_proposals
            .values()
            .filter_map(target_member_id_of);
        voting.chain(approved).collect()
    }

    /// True iff `approved_proposals` carries any `RemoveMember(member_id)`,
    /// regardless of source. Used by `steward_eligibility` to skip a member
    /// whose removal is queued — MLS forbids them from committing it
    /// themselves. Broader than [`Self::is_pending_self_leave`].
    pub fn has_approved_removal(&self, member_id: &[u8]) -> bool {
        self.approved_proposals
            .values()
            .any(|req| removes_member(req, member_id))
    }

    /// True iff `approved_proposals` already admits `member_id`. Mirrors
    /// [`Self::has_approved_removal`]: an invitation and a sponsored join
    /// request can admit the same member under different proposal ids, and
    /// their key packages need not match — so MLS cannot collapse them into one
    /// proposal and rejects the whole batch.
    pub fn has_approved_invite(&self, member_id: &[u8]) -> bool {
        self.approved_proposals
            .values()
            .any(|req| invites_member(req, member_id))
    }

    /// True if `member_id` has a self-leave waiting for the next commit —
    /// an approved `RemoveMember(member_id)` under the deterministic
    /// self-leave ID. Used by live rotation to skip the leaver.
    pub fn is_pending_self_leave(&self, member_id: &[u8]) -> bool {
        let pid = self_leave_proposal_id(member_id);
        self.approved_proposals
            .get(&pid)
            .is_some_and(|req| removes_member(req, member_id))
    }

    /// Insert a proposal straight into the approved queue, staged for commit.
    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: ConversationUpdateRequest,
    ) {
        self.approved_proposals.insert(proposal_id, proposal);
    }

    /// Clear the approved-proposal queue and return the cleared batch in
    /// FIFO insertion order so callers can archive it for UI / diagnostic
    /// history. Returns an empty `Vec` when the queue was already empty.
    pub fn drain_approved_proposals(&mut self) -> Vec<ConversationUpdateRequest> {
        self.approved_proposals
            .drain(..)
            .map(|(_, req)| req)
            .collect()
    }

    /// Drop every `RemoveMember(target)` entry; other approvals stay
    /// queued for the next normal cycle.
    pub fn drop_approved_removals_for(&mut self, target: &[u8]) {
        self.approved_proposals
            .retain(|_pid, req| !removes_member(req, target));
    }

    // ─────────────────────────── Voting Proposals ───────────────────────────

    /// Record `proposal_id` as in flight, whoever opened it. Idempotent: a
    /// later sighting (e.g. a peer echo of our own proposal) leaves the first
    /// entry untouched.
    pub fn track_voting_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: &ConversationUpdateRequest,
    ) {
        self.voting_proposals
            .entry(proposal_id)
            .or_insert_with(|| VotingMeta {
                target: in_flight_target(proposal),
                kind: ProposalKind::of(proposal),
            });
    }

    /// True while `proposal_id` is in flight (in the voting queue).
    #[cfg(test)]
    pub fn is_voting(&self, proposal_id: ProposalId) -> bool {
        self.voting_proposals.contains_key(&proposal_id)
    }

    /// Drop a proposal from the voting queue (resolved, rejected, or deduped).
    pub fn remove_voting_proposal(&mut self, proposal_id: ProposalId) {
        self.voting_proposals.remove(&proposal_id);
    }

    /// Cheap idempotence check for auto-retry: don't submit a second election
    /// while one — own or a peer's — is still being voted on.
    pub fn has_election_in_flight(&self) -> bool {
        self.voting_proposals
            .values()
            .any(|m| m.kind.is_steward_election())
    }

    // ─────────────────────────── Emergency (RFC §Partial Freeze) ───────────────────────────

    /// Record an active emergency criteria proposal.
    pub fn insert_emergency(&mut self, proposal_id: ProposalId) {
        self.active_emergency_ids.insert(proposal_id);
    }

    /// Mark an emergency criteria proposal as finalized.
    pub fn remove_emergency(&mut self, proposal_id: ProposalId) {
        self.active_emergency_ids.remove(&proposal_id);
    }

    /// Check if any emergency criteria proposal is active (partial freeze).
    pub fn has_active_emergency(&self) -> bool {
        !self.active_emergency_ids.is_empty()
    }

    /// RFC §Partial Freeze: while an emergency is active, proposals of
    /// strictly lower priority MUST be blocked. Returns `true` when `kind`
    /// should be rejected under the current freeze state.
    pub fn partial_freeze_blocks(&self, kind: ProposalKind) -> bool {
        self.has_active_emergency() && kind < ProposalKind::Emergency
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

    // ─────────────────────────── Pending Update Buffer ───────────────────────────

    /// Insert a `ConversationUpdateRequest` into the pending-updates buffer.
    ///
    /// Keyed by target user — a second insertion for the same user
    /// keeps the original `first_seen_epoch` so it can still expire on schedule.
    /// Returns `true` if this is a new entry, `false` if already buffered.
    pub fn insert_pending_update(
        &mut self,
        request: ConversationUpdateRequest,
        current_epoch: u64,
    ) -> bool {
        let Some(key) = target_member_id_of(&request) else {
            return false;
        };
        if self.pending_updates.contains_key(&key) {
            return false;
        }
        self.pending_updates.insert(
            key,
            PendingUpdate {
                request,
                first_seen_epoch: current_epoch,
            },
        );
        true
    }

    /// Drop a buffered update by target member_id.
    ///
    /// Returns `true` if an entry was removed.
    pub fn remove_pending_update(&mut self, member_id: &[u8]) -> bool {
        self.pending_updates.remove(member_id).is_some()
    }

    /// Check whether a pending update exists for the given member_id.
    #[cfg(test)]
    pub fn has_pending_update(&self, member_id: &[u8]) -> bool {
        self.pending_updates.contains_key(member_id)
    }

    /// Read-only access to the pending-updates buffer.
    pub fn pending_updates(&self) -> &HashMap<Vec<u8>, PendingUpdate> {
        &self.pending_updates
    }

    /// Number of buffered pending updates.
    pub fn pending_update_count(&self) -> usize {
        self.pending_updates.len()
    }

    /// Drop entries whose `first_seen_epoch` is older than `current_epoch - max_age`.
    ///
    /// Returns the member ids of expired entries.
    pub fn expire_pending_updates(&mut self, current_epoch: u64, max_age: u32) -> Vec<Vec<u8>> {
        let cutoff = current_epoch.saturating_sub(max_age as u64);
        let expired: Vec<Vec<u8>> = self
            .pending_updates
            .iter()
            .filter(|(_, p)| p.first_seen_epoch < cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &expired {
            self.pending_updates.remove(k);
        }
        expired
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

/// True iff `req` is a `RemoveMember` targeting `member_id`.
fn removes_member(req: &ConversationUpdateRequest, member_id: &[u8]) -> bool {
    matches!(
        req.payload.as_ref(),
        Some(conversation_update_request::Payload::RemoveMember(r)) if r.member_id == member_id
    )
}

/// True when `req` admits `member_id`, whatever key package it carries.
fn invites_member(req: &ConversationUpdateRequest, member_id: &[u8]) -> bool {
    matches!(
        req.payload.as_ref(),
        Some(conversation_update_request::Payload::MemberInvite(_))
    ) && target_member_id_of(req).is_some_and(|target| target == member_id)
}

/// Capacity of the resolved-outcome dedup set (`resolved_proposals`): proposal
/// IDs with a locally observed consensus outcome, kept for the duplicate-drop
/// guard on consensus outcomes and the late-vote classifier on incoming votes.
/// Sized well above the consensus library's `max_sessions_per_scope` (default
/// 10) so a late vote arriving within any plausible peer-lag window still finds
/// its id cached.
const RESOLVED_PROPOSAL_CACHE_CAPACITY: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::types::MemberId;
    use crate::protos::de_mls::messages::v1::ViolationEvidence;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn remove_request(target: &[u8]) -> ConversationUpdateRequest {
        ConversationUpdateRequest::remove_member(target.to_vec())
    }

    fn candidate(tag: u8, sender: &[u8], epoch: u64) -> EarlyCandidate {
        let commit = vec![tag; 8];
        EarlyCandidate {
            hash: CommitHash::of(&commit),
            envelope: CommitCandidate {
                conversation_id: b"g".to_vec(),
                commit_message: commit,
                steward_member_id: sender.to_vec(),
                proposal_count: 1,
            },
            facts: StagedFacts {
                sender: MemberId::from(sender),
                epoch,
                actions: Vec::new(),
                proposal_count: 1,
                self_removed: false,
            },
        }
    }

    #[test]
    fn resolved_cache_records_and_evicts_fifo() {
        let mut cache = BoundedSet::new(3);
        cache.insert(1);
        cache.insert(2);
        cache.insert(3);
        assert!(cache.contains(&1));
        assert!(cache.contains(&2));
        assert!(cache.contains(&3));

        cache.insert(4);
        assert!(!cache.contains(&1), "oldest entry must be evicted");
        assert!(cache.contains(&2));
        assert!(cache.contains(&3));
        assert!(cache.contains(&4));
    }

    #[test]
    fn resolved_cache_dedupes_and_does_not_bump_position() {
        let mut cache = BoundedSet::new(3);
        cache.insert(1);
        cache.insert(2);
        cache.insert(1); // no-op: 1 stays in its original slot
        cache.insert(3);
        assert!(cache.contains(&1));
        assert!(cache.contains(&2));
        assert!(cache.contains(&3));

        // Fourth distinct id evicts the oldest (1), not a later entry.
        cache.insert(4);
        assert!(!cache.contains(&1));
        assert!(cache.contains(&2));
        assert!(cache.contains(&3));
        assert!(cache.contains(&4));
    }

    #[test]
    fn bounded_set_clamps_zero_capacity_so_dedup_still_works() {
        let mut cache = BoundedSet::new(0);
        assert!(cache.insert(1), "first insert is new");
        assert!(
            cache.contains(&1),
            "capacity 0 must clamp to 1, not disable dedup"
        );
        assert!(!cache.insert(1), "duplicate must be rejected");
    }

    #[test]
    fn mark_consensus_outcome_persists_in_resolved_cache() {
        let mut queues = EngineQueues::new(10);
        assert!(!queues.is_consensus_outcome_applied(42));
        queues.mark_consensus_outcome_applied(42);
        assert!(queues.is_consensus_outcome_applied(42));
    }

    fn insert_remove_member(queues: &mut EngineQueues, target: &[u8], proposal_id: ProposalId) {
        queues.insert_approved_proposal(proposal_id, remove_request(target));
    }

    /// `approved_proposals` order is FIFO regardless of proposal-id ordering.
    #[test]
    fn test_approved_proposals_preserve_fifo_across_mutations() {
        let mut queues = EngineQueues::new(10);

        insert_remove_member(&mut queues, &member(2), 500);
        insert_remove_member(&mut queues, &member(3), 100);
        insert_remove_member(&mut queues, &member(4), 300);
        let order: Vec<ProposalId> = queues.approved_proposals().keys().copied().collect();
        assert_eq!(order, vec![500, 100, 300]);

        // Re-inserting an existing id does not duplicate or reorder.
        insert_remove_member(&mut queues, &member(2), 500);
        let order: Vec<ProposalId> = queues.approved_proposals().keys().copied().collect();
        assert_eq!(order, vec![500, 100, 300]);

        queues.drain_approved_proposals();
        assert!(queues.approved_proposals().is_empty());
    }

    #[test]
    fn test_urgent_commit_target_set_take_clears() {
        let mut queues = EngineQueues::new(10);
        assert!(queues.urgent_commit_target().is_none());

        let target = member(7);
        queues.set_urgent_commit_target(target.clone());
        assert_eq!(queues.urgent_commit_target(), Some(target.as_slice()));

        let taken = queues.take_urgent_commit_target().unwrap();
        assert_eq!(taken, target);
        assert!(queues.urgent_commit_target().is_none());
    }

    #[test]
    fn test_drop_approved_removals_for_target() {
        let mut queues = EngineQueues::new(10);
        let victim = member(7);
        let bystander = member(9);

        insert_remove_member(&mut queues, &victim, 100);
        insert_remove_member(&mut queues, &victim, 101);
        insert_remove_member(&mut queues, &bystander, 200);
        assert_eq!(queues.approved_proposals_count(), 3);

        queues.drop_approved_removals_for(&victim);

        assert_eq!(queues.approved_proposals_count(), 1);
        assert!(queues.approved_proposals().contains_key(&200));
        assert!(!queues.approved_proposals().contains_key(&100));
        assert!(!queues.approved_proposals().contains_key(&101));
    }

    fn buffer_remove_at(queues: &mut EngineQueues, target: &[u8], epoch: u64) {
        assert!(queues.insert_pending_update(remove_request(target), epoch));
    }

    /// Reducing `pending_update_max_epochs` (e.g. via a tightened
    /// `ConversationSync`) must expire entries whose age now exceeds the new max.
    /// Cutoff math: `current_epoch - max_age`; entries with
    /// `first_seen_epoch < cutoff` are dropped.
    #[test]
    fn test_expire_pending_updates_drops_entries_older_than_max_age() {
        let mut queues = EngineQueues::new(10);
        let stale = member(7);
        let fresh = member(9);

        buffer_remove_at(&mut queues, &stale, 0);
        buffer_remove_at(&mut queues, &fresh, 4);
        assert_eq!(queues.pending_update_count(), 2);

        let expired = queues.expire_pending_updates(5, 1);

        assert_eq!(expired, vec![stale.clone()]);
        assert_eq!(queues.pending_update_count(), 1);
        assert!(queues.has_pending_update(&fresh));
        assert!(!queues.has_pending_update(&stale));
    }

    /// `max_age = 0` keeps only entries from the current epoch — the
    /// boundary case a tightened sync hits when shrinking the window.
    #[test]
    fn test_expire_pending_updates_max_age_zero_keeps_only_current_epoch() {
        let mut queues = EngineQueues::new(10);
        let prior = member(7);
        let current = member(9);

        buffer_remove_at(&mut queues, &prior, 4);
        buffer_remove_at(&mut queues, &current, 5);

        let expired = queues.expire_pending_updates(5, 0);

        assert_eq!(expired, vec![prior.clone()]);
        assert!(queues.has_pending_update(&current));
        assert!(!queues.has_pending_update(&prior));
    }

    /// A score-below-threshold removal is a live change for its target across
    /// its whole lifecycle: while the ECP is still voting (via the in-flight
    /// index) and after it resolves into a queued `RemoveMember` (via the
    /// approved queue). `active_proposal_targets` sees it the whole time, so a
    /// second score removal for the same target is a no-op rather than a
    /// duplicate round.
    #[test]
    fn score_removal_target_stays_covered_through_its_lifecycle() {
        let mut queues = EngineQueues::new(10);
        let creator = member(1);
        let target = member(7);

        let evidence =
            ViolationEvidence::score_below_threshold(target.clone(), 0, 0).with_creator(creator);
        let request = evidence.into_update_request().unwrap();
        let proposal_id = 300;

        // Voting: the ECP hasn't produced a RemoveMember yet, but the target is
        // already covered — the blind spot the ECP-aware index closes.
        queues.track_voting_proposal(proposal_id, &request);
        assert!(queues.is_voting(proposal_id));
        assert!(
            queues.active_proposal_targets().contains(target.as_slice()),
            "the in-flight ECP already covers its target"
        );
        assert!(!queues.has_approved_removal(&target));

        // Resolved: the ECP transforms into a queued RemoveMember, and the
        // target stays covered with no gap.
        queues.remove_voting_proposal(proposal_id);
        queues.insert_approved_proposal(proposal_id, remove_request(&target));
        assert!(queues.has_approved_removal(&target));
        assert!(
            queues.active_proposal_targets().contains(target.as_slice()),
            "the queued RemoveMember still covers the target"
        );
    }

    /// A removal targeting a member also makes them steward-ineligible, so the
    /// list never picks a member MLS forbids from committing its own removal.
    #[test]
    fn steward_eligibility_skips_non_members_and_queued_removals() {
        let mut queues = EngineQueues::new(10);
        let victim = member(7);
        let bystander = member(9);
        let outsider = member(11);
        insert_remove_member(&mut queues, &victim, 100);

        let members = vec![victim.clone(), bystander.clone()];
        let eligible = queues.steward_eligibility(&members);
        assert!(eligible(&bystander));
        assert!(!eligible(&victim));
        assert!(!eligible(&outsider));
    }

    /// The join epoch is per member id and survives every later commit, so a
    /// sync recomputing the unsettled set at a past epoch gets the same answer.
    /// A departure drops the entry, so a re-join is unsettled again.
    #[test]
    fn membership_bookkeeping_tracks_join_epochs_and_clears_departures() {
        let mut queues = EngineQueues::new(10);
        let joiner = member(2);
        let leaver = member(3);

        // The leaver is buffered and queued for removal before the commit lands.
        buffer_remove_at(&mut queues, &leaver, 4);
        insert_remove_member(&mut queues, &leaver, 100);
        queues.member_join_epoch.insert(leaver.clone(), 1);

        queues.store_membership_delta(MembershipDelta {
            added: vec![joiner.clone()],
            removed: vec![leaver.clone()],
        });
        queues.apply_membership_delta_bookkeeping(5);

        assert!(!queues.has_pending_update(&leaver));
        assert!(!queues.has_approved_removal(&leaver));
        assert!(queues.is_settled(&leaver, 5), "an untracked id is settled");

        assert!(
            !queues.is_settled(&joiner, 5),
            "unsettled in its join epoch"
        );
        assert!(queues.is_settled(&joiner, 6));
        assert_eq!(
            queues.settled_members(std::slice::from_ref(&joiner), 6),
            vec![joiner]
        );

        // The delta stays available for the reconcile step that consumes it.
        let delta = queues.take_membership_delta();
        assert_eq!(delta.added.len(), 1);
        assert!(queues.take_membership_delta().is_empty());
    }

    /// A buffered invite is keyed by the joiner's id, which is unchanged by
    /// seating — so the commit that seats them drops the buffer entry.
    #[test]
    fn membership_bookkeeping_drops_the_buffered_update_for_a_seated_joiner() {
        let mut queues = EngineQueues::new(10);
        let joiner = member(2);

        // A removal request stands in for any buffered update keyed by target.
        buffer_remove_at(&mut queues, &joiner, 4);
        queues.store_membership_delta(MembershipDelta {
            added: vec![joiner.clone()],
            removed: Vec::new(),
        });
        queues.apply_membership_delta_bookkeeping(5);

        assert!(!queues.has_pending_update(&joiner));
    }

    /// The stash keeps one entry per commit hash, holds only the current
    /// epoch's candidates, and stops at `max`.
    #[test]
    fn early_candidates_dedupe_by_hash_and_drop_stale_epochs() {
        let mut queues = EngineQueues::new(10);
        let steward = member(2);

        queues.stash_early_candidate(5, candidate(1, &steward, 5), 2);
        queues.stash_early_candidate(5, candidate(1, &steward, 5), 2);
        assert_eq!(queues.take_early_candidates(5).len(), 1, "deduped by hash");

        queues.stash_early_candidate(5, candidate(1, &steward, 5), 2);
        queues.stash_early_candidate(5, candidate(2, &steward, 5), 2);
        queues.stash_early_candidate(5, candidate(3, &steward, 5), 2);
        assert_eq!(queues.take_early_candidates(5).len(), 2, "capped at max");

        // A candidate tagged with an older epoch is dropped by the next stash.
        queues.stash_early_candidate(5, candidate(1, &steward, 5), 2);
        queues.stash_early_candidate(6, candidate(2, &steward, 6), 2);
        let taken = queues.take_early_candidates(6);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].facts.epoch, 6);
        assert!(queues.take_early_candidates(6).is_empty(), "stash cleared");
    }
}
