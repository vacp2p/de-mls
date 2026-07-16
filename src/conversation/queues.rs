//! Per-conversation protocol-queue state: approved/voting proposal queues,
//! commit-round candidate buffer, pending-update buffer, urgent-commit
//! target, ECP dedup.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use indexmap::{IndexMap, IndexSet};

use crate::{
    commit_round::{CommitHash, CommitRoundBuffer},
    conversation::util::{
        is_auto_approved_entry, member_set, self_leave_proposal_id, target_member_id_of,
    },
    proposal_kind::ProposalKind,
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, conversation_update_request,
    },
};

/// Consensus proposal identifier (assigned by the consensus service).
pub type ProposalId = u32;

/// Represents a pending membership update that may not yet be committed.
///
/// Buffered by each member. If the epoch steward doesn't commit the change,
/// the next steward can handle it. Removed once applied or after a set number
/// of epochs since first seen.
#[derive(Clone, Debug)]
pub struct PendingUpdate {
    pub request: ConversationUpdateRequest,
    /// MLS epoch at which this update was first observed locally.
    pub first_seen_epoch: u64,
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
/// Construct with [`ConversationQueues::new`].
pub struct ConversationQueues {
    conversation_id: String,
    /// Proposals that passed consensus, waiting for steward to commit.
    /// `IndexMap` so insertion order (FIFO) is preserved without a side
    /// vector — library proposal IDs are content-derived hashes, so
    /// sort-by-id is not temporal.
    approved_proposals: IndexMap<ProposalId, ConversationUpdateRequest>,
    /// Proposals in flight through consensus — ours and peers', treated
    /// identically. It is the record that a change is already being voted on,
    /// which stops the epoch steward re-proposing it out of `pending_updates`
    /// (RFC §Consensus Types: the epoch steward collects and commits YES-voted
    /// proposals; it does not re-create them).
    voting_proposals: HashMap<ProposalId, ConversationUpdateRequest>,
    /// Active emergency criteria proposals not yet finalized by consensus.
    /// While non-empty, lower-priority proposals MUST be blocked (RFC §Partial Freeze).
    active_emergency_ids: HashSet<ProposalId>,
    /// Members with pending score-based removal ECPs (dedup to prevent duplicates).
    pending_removal_targets: HashSet<Vec<u8>>,
    /// Recent commit hashes for dedup.
    committed_batch_hashes: BoundedSet<CommitHash>,
    /// Commit-round candidate buffer for deterministic selection.
    pub(crate) commit_round: CommitRoundBuffer,
    /// Buffer of membership updates (Add/Remove) that every member records so a
    /// future epoch steward can retry them if the current one fails to commit.
    pending_updates: HashMap<Vec<u8>, PendingUpdate>,
    /// Bounded FIFO of proposal IDs with a locally-observed consensus outcome.
    /// Used by `handle_consensus_outcome` to drop library re-emissions and by
    /// `forward_incoming_vote` to distinguish benign late peer votes (session
    /// was trimmed after resolution) from votes for unknown proposal IDs.
    resolved_proposals: BoundedSet<ProposalId>,
    /// When `Some(target)`, the next freeze cycle commits only the
    /// `RemoveMember(target)` entry; other approvals wait so they don't
    /// dilute the fast-removal intent.
    urgent_commit_target: Option<Vec<u8>>,
    /// Commit candidates that arrived before the local node approved their
    /// proposal; replayed once approval lands so the proposer doesn't fall an
    /// epoch behind. Epoch-tagged; stale entries dropped on the next stash/take.
    early_candidates: Vec<(u64, CommitCandidate)>,
    /// Join epoch per member, recorded from each commit's Add set. Drives the
    /// "settled" check (see [`Self::is_settled`]); deterministic across nodes
    /// that apply the same commit.
    member_join_epoch: HashMap<Vec<u8>, u64>,
    /// Hashes of recently-seen welcome broadcasts, so gossip duplicates
    /// emit a single `WelcomeReady` event.
    welcome_broadcast_hashes: BoundedSet<CommitHash>,
    /// Stewards this node observed letting the commit-inactivity window lapse
    /// (the freeze-timeout accusation). Recovery elections and the `Deadlock`
    /// escalation skip them when resolving proposer authority, so an offline
    /// steward can't stay the only member authorized to recover from its own
    /// silence. A member is cleared individually as soon as authenticated
    /// activity from it is observed (a signature-validated vote) — a comeback
    /// restores its authority and candidacy mid-recovery — and the whole set
    /// clears when a commit merges.
    unresponsive_stewards: HashSet<Vec<u8>>,
}

impl ConversationQueues {
    pub fn new(conversation_id: &str, dedup_window: usize) -> Self {
        Self {
            conversation_id: conversation_id.to_string(),
            approved_proposals: IndexMap::new(),
            voting_proposals: HashMap::new(),
            active_emergency_ids: HashSet::new(),
            pending_removal_targets: HashSet::new(),
            committed_batch_hashes: BoundedSet::new(dedup_window),
            commit_round: CommitRoundBuffer::default(),
            pending_updates: HashMap::new(),
            resolved_proposals: BoundedSet::new(RESOLVED_PROPOSAL_CACHE_CAPACITY),
            urgent_commit_target: None,
            early_candidates: Vec::new(),
            member_join_epoch: HashMap::new(),
            welcome_broadcast_hashes: BoundedSet::new(dedup_window),
            unresponsive_stewards: HashSet::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.conversation_id
    }

    pub fn name_bytes(&self) -> &[u8] {
        self.conversation_id.as_bytes()
    }

    /// Build the eligibility predicate that the steward plug-in's "live"
    /// position queries take. A candidate is eligible when they are an
    /// MLS member of the conversation AND don't have a removal queued in
    /// `approved_proposals`. The closure borrows from `self` and
    /// `mls_members` for the lifetime of the call.
    pub fn steward_eligibility(&self, mls_members: &[Vec<u8>]) -> impl Fn(&[u8]) -> bool {
        let mls_set = member_set(mls_members);
        move |candidate: &[u8]| !self.has_approved_removal(candidate) && mls_set.contains(candidate)
    }

    // ─────────────────────────── Settled membership ───────────────────────────

    /// Record the join epoch of each member a just-merged commit added.
    /// Idempotent — keeps the first epoch we saw a member added.
    pub fn note_member_joins(&mut self, batch: &[ConversationUpdateRequest], epoch: u64) {
        for req in batch {
            if let Some(conversation_update_request::Payload::MemberInvite(im)) =
                req.payload.as_ref()
            {
                self.member_join_epoch
                    .entry(im.member_id.clone())
                    .or_insert(epoch);
            }
        }
    }

    /// Settled once the epoch has advanced past the member's join. Unknown
    /// members (present before tracking, e.g. the creator) count as settled.
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
    pub fn active_proposal_targets(&self) -> HashSet<&[u8]> {
        self.voting_proposals
            .values()
            .chain(self.approved_proposals.values())
            .filter_map(target_member_id_of)
            .collect()
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

    /// Drop a single proposal from the approved queue without archiving.
    pub fn remove_approved_proposal(&mut self, proposal_id: ProposalId) {
        self.approved_proposals.shift_remove(&proposal_id);
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

    /// Discard the approved queue on freeze failure. `RemoveMember`
    /// proposals carry settled YES outcomes and survive so a recovered
    /// steward can commit them; other approvals (Add) are dropped.
    pub fn reject_all_approved_proposals(&mut self) {
        self.approved_proposals.retain(|_pid, req| {
            matches!(
                req.payload.as_ref(),
                Some(conversation_update_request::Payload::RemoveMember(_))
            )
        });
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
        proposal: ConversationUpdateRequest,
    ) {
        self.voting_proposals.entry(proposal_id).or_insert(proposal);
    }

    /// True while `proposal_id` is in flight (in the voting queue).
    pub fn is_voting(&self, proposal_id: ProposalId) -> bool {
        self.voting_proposals.contains_key(&proposal_id)
    }

    /// Drop a proposal from the voting queue (resolved, rejected, or deduped).
    pub fn remove_voting_proposal(&mut self, proposal_id: ProposalId) {
        self.voting_proposals.remove(&proposal_id);
    }

    /// Cheap idempotence check for auto-retry: don't submit a second election
    /// while a previous one — own or a peer's — is still being voted on.
    /// Reads the local voting queue and the observed-election set — a
    /// proposal-queue concern, not steward-list state.
    pub fn has_election_in_flight(&self) -> bool {
        self.voting_proposals
            .values()
            .any(|req| ProposalKind::of(req).is_steward_election())
    }

    // ─────────────────────────── Unresponsive stewards ───────────────────────────

    /// Record a locally-observed unresponsive steward (the freeze-timeout
    /// accusation target).
    pub fn note_unresponsive_steward(&mut self, member_id: Vec<u8>) {
        self.unresponsive_stewards.insert(member_id);
    }

    /// Whether this node observed `member_id` letting the commit-inactivity
    /// window lapse since the last merged commit.
    pub fn is_unresponsive_steward(&self, member_id: &[u8]) -> bool {
        self.unresponsive_stewards.contains(member_id)
    }

    /// Authenticated activity observed from `member_id` — it is back, so the
    /// accusation no longer holds and its authority/candidacy return.
    pub fn clear_unresponsive_steward(&mut self, member_id: &[u8]) {
        self.unresponsive_stewards.remove(member_id);
    }

    /// A commit merged — the accusations served their purpose; a steward that
    /// is still silent gets re-accused by the next freeze timeout.
    pub fn clear_unresponsive_stewards(&mut self) {
        self.unresponsive_stewards.clear();
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

    // ─────────────────────────── Removal Dedup ───────────────────────────

    /// Record that a score-based removal ECP has been submitted for this member.
    pub fn insert_pending_removal(&mut self, member_id: Vec<u8>) {
        self.pending_removal_targets.insert(member_id);
    }

    /// Check if a removal ECP is already pending for this member.
    pub fn has_pending_removal(&self, member_id: &[u8]) -> bool {
        self.pending_removal_targets.contains(member_id)
    }

    /// Mark a removal ECP as finalized (resolved regardless of outcome).
    pub fn remove_pending_removal(&mut self, member_id: &[u8]) {
        self.pending_removal_targets.remove(member_id);
    }

    // ─────────────────────────── Committed-Hash Dedup ───────────────────────────

    /// Check if a commit hash has already been committed (in committed history).
    ///
    /// Note: commit round buffer dedup is handled separately by `CommitRoundBuffer::add`.
    pub(crate) fn has_committed_hash(&self, commit_hash: &CommitHash) -> bool {
        self.committed_batch_hashes.contains(commit_hash)
    }

    /// Record a committed batch's hash for future dedup.
    pub(crate) fn insert_committed_hash(&mut self, commit_hash: CommitHash) {
        self.committed_batch_hashes.insert(commit_hash);
    }

    /// Record a welcome broadcast's hash. Returns `true` when the hash is
    /// new (process it) and `false` for a duplicate (drop it).
    pub(crate) fn record_welcome_broadcast(&mut self, hash: CommitHash) -> bool {
        self.welcome_broadcast_hashes.insert(hash)
    }

    // ──────────────────────── Early (pre-approval) Candidates ────────────────────────

    /// Stash a commit candidate that arrived before its proposal was locally
    /// approved. Deduped by commit message and capped at `max`. Entries tagged
    /// with a different epoch are dropped — the node has moved on.
    pub fn stash_early_candidate(&mut self, epoch: u64, candidate: CommitCandidate, max: usize) {
        self.early_candidates.retain(|(e, _)| *e == epoch);
        let duplicate = self
            .early_candidates
            .iter()
            .any(|(_, c)| c.commit_message == candidate.commit_message);
        if duplicate || self.early_candidates.len() >= max {
            return;
        }
        self.early_candidates.push((epoch, candidate));
    }

    /// Remove and return stashed candidates for `epoch`, discarding any tagged
    /// with a different (stale) epoch. Clears the stash.
    pub fn take_early_candidates(&mut self, epoch: u64) -> Vec<CommitCandidate> {
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
        let Some(member_id) = target_member_id_of(&request) else {
            return false;
        };
        let key = member_id.to_vec();
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
    /// Returns the identities of expired entries for logging.
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

    /// Drop Add entries whose target is now a conversation member, and Remove entries
    /// whose target is no longer a conversation member. Call after a commit has merged.
    ///
    /// Also drops any auto-approved self-leave in `approved_proposals` whose
    /// target is no longer a conversation member (the leave has been applied).
    /// Regular approved entries are normally drained by
    /// `drain_approved_proposals` on the same commit path; this is an extra
    /// sweep to catch auto-approved entries that survived freeze failures.
    pub fn prune_pending_updates_for_members(&mut self, current_members: &[Vec<u8>]) {
        let in_conversation: HashSet<&Vec<u8>> = current_members.iter().collect();
        self.member_join_epoch
            .retain(|member_id, _| in_conversation.contains(member_id));
        self.pending_updates.retain(|member_id, entry| {
            match entry.request.payload.as_ref() {
                Some(conversation_update_request::Payload::MemberInvite(_)) => {
                    !in_conversation.contains(&member_id)
                }
                Some(conversation_update_request::Payload::RemoveMember(_)) => {
                    in_conversation.contains(&member_id)
                }
                // Only Invite/Remove are ever buffered; anything else is a
                // bug — surface it rather than drop silently.
                other => {
                    tracing::error!(
                        ?other,
                        "non-membership payload in pending_updates buffer (invariant violation)"
                    );
                    false
                }
            }
        });

        // Sweep orphaned auto-approved leaves (target no longer in conversation).
        self.approved_proposals.retain(|pid, req| {
            if !is_auto_approved_entry(*pid, req) {
                return true;
            }
            match req.payload.as_ref() {
                Some(conversation_update_request::Payload::RemoveMember(r)) => {
                    in_conversation.contains(&r.member_id)
                }
                _ => true,
            }
        });

        // Drop removal-dedup entries for targets no longer present: a leaked
        // entry (ECP that never resolved) would otherwise block them forever.
        self.pending_removal_targets
            .retain(|target| in_conversation.contains(target));
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
        Some(conversation_update_request::Payload::MemberInvite(m)) if m.member_id == member_id
    )
}

/// Capacity of the resolved-outcome dedup set (`resolved_proposals`): proposal
/// IDs with a locally observed consensus outcome, kept for the duplicate-drop
/// guard in `handle_consensus_outcome` and the late-vote classifier in
/// `forward_incoming_vote`. Sized well above the consensus library's
/// `max_sessions_per_scope` (default 10) so a late vote arriving within any
/// plausible peer-lag window still finds its id cached.
const RESOLVED_PROPOSAL_CACHE_CAPACITY: usize = 256;

/// Test-only stubs shared across `core/`'s unit-test modules.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::de_mls::messages::v1::MemberInvite;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    fn insert_self_leave(conversation: &mut ConversationQueues, member_id: &[u8]) {
        let remove = ConversationUpdateRequest::remove_member(member_id.to_vec());
        conversation.insert_approved_proposal(self_leave_proposal_id(member_id), remove);
    }

    #[test]
    fn unresponsive_accusation_lifts_on_comeback_and_on_commit() {
        let mut conversation = ConversationQueues::new("test-conversation", 10);
        conversation.note_unresponsive_steward(member(1));
        conversation.note_unresponsive_steward(member(2));
        assert!(conversation.is_unresponsive_steward(&member(1)));

        // Authenticated activity from member 1 — its accusation alone lifts.
        conversation.clear_unresponsive_steward(&member(1));
        assert!(!conversation.is_unresponsive_steward(&member(1)));
        assert!(conversation.is_unresponsive_steward(&member(2)));

        // A merged commit clears the rest.
        conversation.clear_unresponsive_stewards();
        assert!(!conversation.is_unresponsive_steward(&member(2)));
    }

    #[test]
    fn test_prune_clears_self_leave_entry_when_member_gone() {
        let mut conversation = ConversationQueues::new("test-conversation", 10);

        let leaver = member(2);
        insert_self_leave(&mut conversation, &leaver);
        assert_eq!(conversation.approved_proposals_count(), 1);
        assert!(conversation.is_pending_self_leave(&leaver));

        // Commit merged — leaver is no longer a member.
        let after = members(&[1, 3]);
        conversation.prune_pending_updates_for_members(&after);

        assert_eq!(conversation.approved_proposals_count(), 0);
        assert!(!conversation.is_pending_self_leave(&leaver));
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
        let mut conversation = ConversationQueues::new("g", 10);
        assert!(!conversation.is_consensus_outcome_applied(42));
        conversation.mark_consensus_outcome_applied(42);
        assert!(conversation.is_consensus_outcome_applied(42));
    }

    fn insert_remove_member(
        conversation: &mut ConversationQueues,
        target: &[u8],
        proposal_id: ProposalId,
    ) {
        let remove = ConversationUpdateRequest::remove_member(target.to_vec());
        conversation.insert_approved_proposal(proposal_id, remove);
    }

    /// `RemoveMember` proposals survive a freeze failure regardless of
    /// source; Add proposals are dropped.
    #[test]
    fn test_reject_all_approved_preserves_all_remove_member() {
        let mut conversation = ConversationQueues::new("test-conversation", 10);

        let ban_id: ProposalId = 0x1111_2222;
        let ecp_id: ProposalId = 0x3333_4444;
        let add_id: ProposalId = 0x5555_6666;
        insert_remove_member(&mut conversation, &member(2), ban_id);
        insert_remove_member(&mut conversation, &member(3), ecp_id);
        let add = ConversationUpdateRequest::member_invite(MemberInvite {
            key_package_bytes: vec![0; 8],
            member_id: member(99),
        });
        conversation.insert_approved_proposal(add_id, add);
        assert_eq!(conversation.approved_proposals_count(), 3);

        conversation.reject_all_approved_proposals();

        assert_eq!(conversation.approved_proposals_count(), 2);
        assert!(conversation.approved_proposals().contains_key(&ban_id));
        assert!(conversation.approved_proposals().contains_key(&ecp_id));
        assert!(!conversation.approved_proposals().contains_key(&add_id));
    }

    /// `approved_order` is FIFO regardless of proposal-id ordering.
    #[test]
    fn test_approved_proposals_preserve_fifo_across_mutations() {
        let mut conversation = ConversationQueues::new("g", 10);

        insert_remove_member(&mut conversation, &member(2), 500);
        insert_remove_member(&mut conversation, &member(3), 100);
        insert_remove_member(&mut conversation, &member(4), 300);
        let order: Vec<ProposalId> = conversation.approved_proposals().keys().copied().collect();
        assert_eq!(order, vec![500, 100, 300]);

        conversation.remove_approved_proposal(100);
        let order: Vec<ProposalId> = conversation.approved_proposals().keys().copied().collect();
        assert_eq!(order, vec![500, 300]);

        // Re-inserting an existing id does not duplicate or reorder.
        insert_remove_member(&mut conversation, &member(2), 500);
        let order: Vec<ProposalId> = conversation.approved_proposals().keys().copied().collect();
        assert_eq!(order, vec![500, 300]);

        conversation.drain_approved_proposals();
        assert!(conversation.approved_proposals().is_empty());
    }

    #[test]
    fn test_urgent_commit_target_set_take_clears() {
        let mut conversation = ConversationQueues::new("g", 10);
        assert!(conversation.urgent_commit_target().is_none());

        let target = member(7);
        conversation.set_urgent_commit_target(target.clone());
        assert_eq!(conversation.urgent_commit_target(), Some(target.as_slice()));

        let taken = conversation.take_urgent_commit_target().unwrap();
        assert_eq!(taken, target);
        assert!(conversation.urgent_commit_target().is_none());
    }

    #[test]
    fn test_drop_approved_removals_for_target() {
        let mut conversation = ConversationQueues::new("g", 10);
        let victim = member(7);
        let bystander = member(9);

        insert_remove_member(&mut conversation, &victim, 100);
        insert_remove_member(&mut conversation, &victim, 101);
        insert_remove_member(&mut conversation, &bystander, 200);
        assert_eq!(conversation.approved_proposals_count(), 3);

        conversation.drop_approved_removals_for(&victim);

        assert_eq!(conversation.approved_proposals_count(), 1);
        assert!(conversation.approved_proposals().contains_key(&200));
        assert!(!conversation.approved_proposals().contains_key(&100));
        assert!(!conversation.approved_proposals().contains_key(&101));
    }

    fn buffer_remove_at(conversation: &mut ConversationQueues, target: &[u8], epoch: u64) {
        let request = ConversationUpdateRequest::remove_member(target.to_vec());
        assert!(conversation.insert_pending_update(request, epoch));
    }

    /// Reducing `pending_update_max_epochs` (e.g. via a tightened
    /// `ConversationSync`) must expire entries whose age now exceeds the new max.
    /// Cutoff math: `current_epoch - max_age`; entries with
    /// `first_seen_epoch < cutoff` are dropped.
    #[test]
    fn test_expire_pending_updates_drops_entries_older_than_max_age() {
        let mut conversation = ConversationQueues::new("g", 10);
        let stale = member(7);
        let fresh = member(9);

        buffer_remove_at(&mut conversation, &stale, 0);
        buffer_remove_at(&mut conversation, &fresh, 4);
        assert_eq!(conversation.pending_update_count(), 2);

        let expired = conversation.expire_pending_updates(5, 1);

        assert_eq!(expired, vec![stale.clone()]);
        assert_eq!(conversation.pending_update_count(), 1);
        assert!(conversation.has_pending_update(&fresh));
        assert!(!conversation.has_pending_update(&stale));
    }

    /// `max_age = 0` keeps only entries from the current epoch — the
    /// boundary case a tightened sync hits when shrinking the window.
    #[test]
    fn test_expire_pending_updates_max_age_zero_keeps_only_current_epoch() {
        let mut conversation = ConversationQueues::new("g", 10);
        let prior = member(7);
        let current = member(9);

        buffer_remove_at(&mut conversation, &prior, 4);
        buffer_remove_at(&mut conversation, &current, 5);

        let expired = conversation.expire_pending_updates(5, 0);

        assert_eq!(expired, vec![prior.clone()]);
        assert!(conversation.has_pending_update(&current));
        assert!(!conversation.has_pending_update(&prior));
    }

    /// `observe → has → resolve` cycle for `pending_removal_targets`,
    /// covering the idempotent re-observe path.
    #[test]
    fn pending_removal_target_observe_resolve_cycle() {
        let mut conversation = ConversationQueues::new("dedup", 10);
        let target = member(10);

        assert!(!conversation.has_pending_removal(&target));

        conversation.insert_pending_removal(target.clone());
        assert!(conversation.has_pending_removal(&target));

        conversation.insert_pending_removal(target.clone());
        assert!(
            conversation.has_pending_removal(&target),
            "second observe is idempotent"
        );

        conversation.remove_pending_removal(&target);
        assert!(!conversation.has_pending_removal(&target));
    }

    /// Once an ECP score-below-threshold YES has resolved into a queued
    /// `RemoveMember`, `has_approved_removal` flips true (it's in the approved
    /// queue) and `has_pending_removal` flips false (the in-flight ECP dedup
    /// is cleared). Both gates together must keep the steward from
    /// re-proposing for the same target.
    #[test]
    fn below_threshold_target_queued_for_removal_is_not_re_proposed() {
        use crate::apply_consensus_result;
        use crate::protos::de_mls::messages::v1::ViolationEvidence;

        let mut conversation = ConversationQueues::new("removal-no-duplicate", 10);
        let creator = member(1);
        let target = member(7);

        let evidence =
            ViolationEvidence::score_below_threshold(target.clone(), 0, 0).with_creator(creator);
        let request = evidence.into_update_request().unwrap();
        let proposal_id = 300;
        conversation.track_voting_proposal(proposal_id, request.clone());
        conversation.insert_pending_removal(target.clone());

        apply_consensus_result(&mut conversation, proposal_id, true, &request).unwrap();
        // Mirror the coordinator: clear the in-flight ECP dedup on resolution.
        conversation.remove_pending_removal(&target);

        assert!(
            conversation.has_approved_removal(&target),
            "RemoveMember should be queued in approved_proposals"
        );
        assert!(
            !conversation.has_pending_removal(&target),
            "in-flight ECP dedup is cleared once the ECP resolves"
        );
    }
}
