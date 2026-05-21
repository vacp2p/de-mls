//! Per-conversation protocol-queue state: approved/voting proposal queues,
//! freeze-round candidate buffer, pending-update buffer, urgent-commit
//! target, ECP dedup. MLS crypto state, the operating mode, and the
//! steward-list plug-in live alongside on `ConversationHandle`.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    core::{
        conversation::util::{
            is_auto_approved_entry, member_set, self_leave_proposal_id, target_identity_of,
        },
        freeze::CommitHash,
        proposal_kind::ProposalKind,
    },
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, conversation_update_request,
    },
};

/// Consensus proposal identifier (assigned by the consensus service).
pub type ProposalId = u32;

/// A membership update that has been observed but may not yet have been committed.
///
/// Every member buffers these so that, if the epoch steward fails to commit the
/// change, the next epoch steward can pick it up. Entries are pruned once the
/// change has been applied to the conversation (member added/removed) or after
/// `max_age_epochs` epochs have elapsed since the entry was first seen.
#[derive(Clone, Debug)]
pub struct PendingUpdate {
    pub request: ConversationUpdateRequest,
    /// MLS epoch at which this update was first observed locally.
    pub first_seen_epoch: u64,
}

const MAX_COMMITTED_HASHES: usize = 10;

/// A commit candidate buffered during freeze for later selection.
#[derive(Clone, Debug)]
pub struct BufferedCommitCandidate {
    pub candidate_msg: CommitCandidate,
    pub commit_hash: CommitHash,
    pub is_local_candidate: bool,
    pub welcome_bytes: Option<Vec<u8>>,
}

/// In-memory freeze-round state for deterministic selection.
#[derive(Clone, Debug)]
pub(crate) struct FreezeRound {
    pub epoch: u64,
    pub selection_locked: bool,
    pub candidates: Vec<BufferedCommitCandidate>,
}

/// Result of [`Conversation::add_freeze_candidate`]. Each non-`Buffered`
/// variant is a legitimate runtime state, not an error: the round may be
/// past the buffer phase, the caller may be on a stale epoch, or the
/// commit hash may already be buffered (peer race / retransmit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezeBufferOutcome {
    /// Candidate stored in the buffer for this epoch.
    Buffered,
    /// Selection has been finalized for this round; the buffer no longer
    /// accepts candidates.
    SelectionLocked,
    /// The caller's `epoch` doesn't match the buffered round's epoch
    /// (e.g. the local node hasn't merged the latest commit yet).
    StaleEpoch,
    /// A candidate with the same commit hash is already buffered.
    DuplicateHash,
}

/// Per-conversation protocol state. Stewards batch commits; members vote.
/// Construct with [`Conversation::new`].
pub struct Conversation {
    conversation_name: String,
    /// Proposals that passed consensus, waiting for steward to commit.
    approved_proposals: HashMap<ProposalId, ConversationUpdateRequest>,
    /// Insertion order of `approved_proposals` (FIFO). Library proposal
    /// IDs are content-derived hashes, so sort-by-id is not temporal.
    approved_order: Vec<ProposalId>,
    /// Proposals waiting on consensus voting (created by this user).
    voting_proposals: HashMap<ProposalId, ConversationUpdateRequest>,
    /// Active emergency criteria proposals not yet finalized by consensus.
    /// While non-empty, lower-priority proposals MUST be blocked (RFC §Partial Freeze).
    active_emergency_ids: HashSet<ProposalId>,
    /// Members with pending score-based removal ECPs (dedup to prevent duplicates).
    pending_removal_targets: HashSet<Vec<u8>>,
    /// Recent commit hashes for dedup.
    committed_batch_hashes: VecDeque<CommitHash>,
    /// Freeze-round candidate buffer for deterministic selection.
    freeze_round: Option<FreezeRound>,
    /// Buffer of membership updates (Add/Remove) that every member records so a
    /// future epoch steward can retry them if the current one fails to commit.
    /// Keyed by target identity so duplicates don't stack.
    pending_updates: HashMap<Vec<u8>, PendingUpdate>,
    /// Bounded FIFO of proposal IDs with a locally-observed consensus outcome.
    /// Used by `apply_consensus_outcome` to drop library re-emissions and by
    /// `forward_incoming_vote` to distinguish benign late peer votes (session
    /// was trimmed after resolution) from votes for unknown proposal IDs.
    resolved_proposals: ResolvedProposalCache,
    /// When `Some(target)`, the next freeze cycle commits only the
    /// `RemoveMember(target)` entry; other approvals wait so they don't
    /// dilute the fast-removal intent.
    urgent_commit_target: Option<Vec<u8>>,
}

impl Conversation {
    pub fn new(conversation_name: &str) -> Self {
        Self {
            conversation_name: conversation_name.to_string(),
            approved_proposals: HashMap::new(),
            approved_order: Vec::new(),
            voting_proposals: HashMap::new(),
            active_emergency_ids: HashSet::new(),
            pending_removal_targets: HashSet::new(),
            committed_batch_hashes: VecDeque::new(),
            freeze_round: None,
            pending_updates: HashMap::new(),
            resolved_proposals: ResolvedProposalCache::new(RESOLVED_PROPOSAL_CACHE_CAPACITY),
            urgent_commit_target: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.conversation_name
    }

    pub fn name_bytes(&self) -> &[u8] {
        self.conversation_name.as_bytes()
    }

    /// Build the eligibility predicate that the steward plug-in's "live"
    /// position queries take. A candidate is eligible when they are an
    /// MLS member of the conversation AND don't have a removal queued in
    /// `approved_proposals`. The closure borrows from `self` and
    /// `mls_members` for the lifetime of the call.
    pub fn steward_eligibility<'a>(
        &'a self,
        mls_members: &'a [Vec<u8>],
    ) -> impl Fn(&[u8]) -> bool + 'a {
        let mls_set = member_set(mls_members);
        move |candidate: &[u8]| !self.is_pending_removal(candidate) && mls_set.contains(candidate)
    }

    /// True iff `approved_proposals` carries any `RemoveMember(identity)`,
    /// regardless of source. Used by `steward_eligibility` to skip a member
    /// whose removal is queued — MLS forbids them from committing it
    /// themselves. Broader than [`Self::is_pending_self_leave`].
    pub fn is_pending_removal(&self, identity: &[u8]) -> bool {
        self.approved_proposals.values().any(|req| {
            matches!(
                req.payload.as_ref(),
                Some(conversation_update_request::Payload::RemoveMember(r)) if r.identity == identity
            )
        })
    }

    // ─────────────────────────── Proposal Queues ───────────────────────────

    /// True when this user created `proposal_id` (it's still in the voting queue).
    pub fn is_owner_of_proposal(&self, proposal_id: ProposalId) -> bool {
        self.voting_proposals.contains_key(&proposal_id)
    }

    pub fn approved_proposals_count(&self) -> usize {
        self.approved_proposals.len()
    }

    pub fn approved_proposals(&self) -> &HashMap<ProposalId, ConversationUpdateRequest> {
        &self.approved_proposals
    }

    /// Insertion order of `approved_proposals` (oldest first).
    pub fn approved_order(&self) -> &[ProposalId] {
        &self.approved_order
    }

    /// Move a proposal from the voting queue into the approved queue.
    pub fn mark_proposal_as_approved(&mut self, proposal_id: ProposalId) {
        if let Some(proposal) = self.voting_proposals.remove(&proposal_id) {
            self.push_approved(proposal_id, proposal);
        }
    }

    /// Drop a proposal from the voting queue (rejected or failed consensus).
    pub fn mark_proposal_as_rejected(&mut self, proposal_id: ProposalId) {
        self.voting_proposals.remove(&proposal_id);
    }

    /// Add a newly-created proposal to the voting queue.
    pub fn store_voting_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: ConversationUpdateRequest,
    ) {
        self.voting_proposals.insert(proposal_id, proposal);
    }

    /// Insert a proposal straight into the approved queue (non-owner path).
    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: ConversationUpdateRequest,
    ) {
        self.push_approved(proposal_id, proposal);
    }

    /// Drop a single proposal from the approved queue without archiving.
    pub fn remove_approved_proposal(&mut self, proposal_id: ProposalId) {
        if self.approved_proposals.remove(&proposal_id).is_some() {
            self.approved_order.retain(|pid| *pid != proposal_id);
        }
    }

    /// Clear the approved-proposal queue and return the cleared batch in
    /// FIFO insertion order so callers can archive it for UI / diagnostic
    /// history. Returns an empty `Vec` when the queue was already empty.
    pub fn clear_approved_proposals(&mut self) -> Vec<ConversationUpdateRequest> {
        self.approved_order
            .drain(..)
            .filter_map(|pid| self.approved_proposals.remove(&pid))
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
        self.approved_order
            .retain(|pid| self.approved_proposals.contains_key(pid));
    }

    /// Drop every entry in the voting queue.
    pub fn reject_all_voting_proposals(&mut self) {
        self.voting_proposals.clear();
    }

    // ─────────────────────────── Partial Freeze (RFC §Partial Freeze Semantics) ───────────────────────────

    /// Record an active emergency criteria proposal.
    pub fn observe_emergency(&mut self, proposal_id: ProposalId) {
        self.active_emergency_ids.insert(proposal_id);
    }

    /// Mark an emergency criteria proposal as finalized.
    pub fn resolve_emergency(&mut self, proposal_id: ProposalId) {
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
    pub fn observe_pending_removal(&mut self, member_id: Vec<u8>) {
        self.pending_removal_targets.insert(member_id);
    }

    /// Check if a removal ECP is already pending for this member.
    pub fn has_pending_removal(&self, member_id: &[u8]) -> bool {
        self.pending_removal_targets.contains(member_id)
    }

    /// Mark a removal ECP as finalized (resolved regardless of outcome).
    pub fn resolve_pending_removal(&mut self, member_id: &[u8]) {
        self.pending_removal_targets.remove(member_id);
    }

    // ─────────────────────────── Urgent (ECP-driven) Commit ───────────────────────────

    pub fn urgent_commit_target(&self) -> Option<&[u8]> {
        self.urgent_commit_target.as_deref()
    }

    /// Drop every `RemoveMember(target)` entry; other approvals stay
    /// queued for the next normal cycle.
    pub fn drop_approved_removals_for(&mut self, target: &[u8]) {
        self.approved_proposals.retain(|_pid, req| {
            !matches!(
                req.payload.as_ref(),
                Some(conversation_update_request::Payload::RemoveMember(r)) if r.identity == target
            )
        });
        self.approved_order
            .retain(|pid| self.approved_proposals.contains_key(pid));
    }

    /// Cheap idempotence check for auto-retry: don't submit a second election
    /// while the previous one is still being voted on. Reads the local
    /// voting queue — proposal-queue concern, not steward-list state.
    pub fn has_election_in_flight(&self) -> bool {
        self.voting_proposals
            .values()
            .any(|req| ProposalKind::of(req).is_steward_election())
    }

    // ─────────────────────────── Freeze Round Operations ───────────────────────────

    /// Replace the active freeze round with a fresh one for `epoch`.
    pub fn start_freeze_round(&mut self, epoch: u64) {
        self.freeze_round = Some(self.build_freeze_round(epoch));
    }

    /// Buffer a validated candidate for the active freeze round.
    pub fn add_freeze_candidate(
        &mut self,
        candidate: BufferedCommitCandidate,
        epoch: u64,
    ) -> FreezeBufferOutcome {
        self.ensure_freeze_round(epoch);
        let Some(round) = self.freeze_round.as_mut() else {
            return FreezeBufferOutcome::StaleEpoch;
        };

        if round.epoch != epoch {
            return FreezeBufferOutcome::StaleEpoch;
        }
        if round.selection_locked {
            return FreezeBufferOutcome::SelectionLocked;
        }

        if round
            .candidates
            .iter()
            .any(|c| c.commit_hash == candidate.commit_hash)
        {
            return FreezeBufferOutcome::DuplicateHash;
        }

        round.candidates.push(candidate);
        FreezeBufferOutcome::Buffered
    }

    /// Get the number of buffered commit candidates in the active freeze round.
    pub fn freeze_candidate_count(&self) -> usize {
        self.freeze_round
            .as_ref()
            .map(|r| r.candidates.len())
            .unwrap_or(0)
    }

    // ─────────────────────────── Pending Update Buffer ───────────────────────────

    /// Insert a `ConversationUpdateRequest` into the pending-updates buffer.
    ///
    /// Keyed by target identity — a second insertion for the same identity
    /// keeps the original `first_seen_epoch` so it can still expire on schedule.
    /// Returns `true` if this is a new entry, `false` if already buffered.
    pub fn buffer_pending_update(
        &mut self,
        request: ConversationUpdateRequest,
        current_epoch: u64,
    ) -> bool {
        let Some(identity) = target_identity_of(&request) else {
            return false;
        };
        let key = identity.to_vec();
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

    /// True if `identity` has a self-leave waiting for the next commit —
    /// an approved `RemoveMember(identity)` under the deterministic
    /// self-leave ID. Used by live rotation to skip the leaver.
    pub fn is_pending_self_leave(&self, identity: &[u8]) -> bool {
        let pid = self_leave_proposal_id(identity);
        self.approved_proposals
            .get(&pid)
            .is_some_and(|req| is_auto_approved_entry(pid, req))
    }

    /// Drop a buffered update by target identity.
    ///
    /// Returns `true` if an entry was removed.
    pub fn remove_pending_update(&mut self, identity: &[u8]) -> bool {
        self.pending_updates.remove(identity).is_some()
    }

    /// Read-only access to the pending-updates buffer.
    pub fn pending_updates(&self) -> &HashMap<Vec<u8>, PendingUpdate> {
        &self.pending_updates
    }

    /// Number of buffered pending updates.
    pub fn pending_update_count(&self) -> usize {
        self.pending_updates.len()
    }

    /// Check whether a pending update exists for the given identity.
    pub fn has_pending_update(&self, identity: &[u8]) -> bool {
        self.pending_updates.contains_key(identity)
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
    /// `clear_approved_proposals` on the same commit path; this is an extra
    /// sweep to catch auto-approved entries that survived freeze failures.
    pub fn prune_pending_updates_for_members(&mut self, current_members: &[Vec<u8>]) {
        let in_conversation: HashSet<&Vec<u8>> = current_members.iter().collect();
        self.pending_updates.retain(|identity, entry| {
            let payload = match entry.request.payload.as_ref() {
                Some(p) => p,
                None => return false,
            };
            match payload {
                conversation_update_request::Payload::MemberInvite(_) => {
                    !in_conversation.contains(identity)
                }
                conversation_update_request::Payload::RemoveMember(_) => {
                    in_conversation.contains(identity)
                }
                _ => false,
            }
        });

        // Sweep orphaned auto-approved leaves (target no longer in conversation).
        self.approved_proposals.retain(|pid, req| {
            if !is_auto_approved_entry(*pid, req) {
                return true;
            }
            match req.payload.as_ref() {
                Some(conversation_update_request::Payload::RemoveMember(r)) => {
                    in_conversation.contains(&r.identity)
                }
                _ => true,
            }
        });
        self.approved_order
            .retain(|pid| self.approved_proposals.contains_key(pid));
    }

    // ─────────────────────────── Crate-internal ───────────────────────────

    pub(crate) fn is_consensus_outcome_applied(&self, proposal_id: ProposalId) -> bool {
        self.resolved_proposals.contains(proposal_id)
    }

    pub(crate) fn mark_consensus_outcome_applied(&mut self, proposal_id: ProposalId) {
        self.resolved_proposals.record(proposal_id);
    }

    /// Mark the next freeze cycle as urgent and committed-only-for `target`.
    pub(crate) fn set_urgent_commit_target(&mut self, target: Vec<u8>) {
        self.urgent_commit_target = Some(target);
    }

    pub(crate) fn take_urgent_commit_target(&mut self) -> Option<Vec<u8>> {
        self.urgent_commit_target.take()
    }

    /// Initialise a freeze round for `epoch` if none exists or the buffered
    /// one is for a stale epoch.
    pub(crate) fn ensure_freeze_round(&mut self, epoch: u64) {
        if matches!(self.freeze_round, Some(ref round) if round.epoch == epoch) {
            return;
        }
        self.freeze_round = Some(self.build_freeze_round(epoch));
    }

    /// Mark the active freeze round as selection-locked.
    pub(crate) fn lock_freeze_round_selection(&mut self, epoch: u64) {
        if let Some(round) = self.freeze_round.as_mut()
            && round.epoch == epoch
        {
            round.selection_locked = true;
        }
    }

    /// Read-only access to the active freeze round.
    pub(crate) fn freeze_round(&self) -> Option<&FreezeRound> {
        self.freeze_round.as_ref()
    }

    /// Clear freeze-round state.
    pub(crate) fn clear_freeze_round(&mut self) {
        self.freeze_round = None;
    }

    /// Move the active round's candidates out and clear the round.
    /// Returns `None` when no round is active or its epoch doesn't match.
    pub(crate) fn take_round_candidates(
        &mut self,
        epoch: u64,
    ) -> Option<Vec<BufferedCommitCandidate>> {
        let round = self.freeze_round.take()?;
        if round.epoch != epoch {
            self.freeze_round = Some(round);
            return None;
        }
        Some(round.candidates)
    }

    /// Check if a commit hash has already been committed (in committed history).
    ///
    /// Note: freeze round buffer dedup is handled separately by `add_freeze_candidate`.
    pub(crate) fn is_duplicate_commit_candidate(&self, commit_hash: &CommitHash) -> bool {
        self.committed_batch_hashes
            .iter()
            .any(|ch| ch == commit_hash)
    }

    /// Record a committed batch's hash for future dedup.
    pub(crate) fn record_committed_batch(&mut self, commit_hash: CommitHash) {
        if self.committed_batch_hashes.len() >= MAX_COMMITTED_HASHES {
            self.committed_batch_hashes.pop_front();
        }
        self.committed_batch_hashes.push_back(commit_hash);
    }

    // ─────────────────────────── Private ───────────────────────────

    /// Re-inserting an existing id preserves the original position.
    fn push_approved(&mut self, proposal_id: ProposalId, proposal: ConversationUpdateRequest) {
        if self
            .approved_proposals
            .insert(proposal_id, proposal)
            .is_none()
        {
            self.approved_order.push(proposal_id);
        }
    }

    fn build_freeze_round(&self, epoch: u64) -> FreezeRound {
        FreezeRound {
            epoch,
            selection_locked: false,
            candidates: Vec::new(),
        }
    }
}

const RESOLVED_PROPOSAL_CACHE_CAPACITY: usize = 256;

/// Bounded FIFO of proposal IDs for which a local consensus outcome has been
/// observed (`ConsensusReached` or timeout-path resolution). Oldest entries
/// are evicted once `capacity` is reached.
///
/// Serves two callers: (a) duplicate-drop guard in `apply_consensus_outcome`
/// against consensus-library re-emissions, and (b) late-packet classifier in
/// `forward_incoming_vote` — a `SessionNotFound` for an id in this cache is
/// a benign late vote (session was trimmed after we resolved it), while the
/// same error for an id we never saw is suspicious and warrants a warn-log.
///
/// `CAPACITY` is sized well above the consensus library's
/// `max_sessions_per_scope` (default 10) so a late vote arriving within any
/// plausible peer-lag window still finds its id cached.
#[derive(Clone, Debug)]
struct ResolvedProposalCache {
    ids: HashSet<ProposalId>,
    order: VecDeque<ProposalId>,
    capacity: usize,
}

impl ResolvedProposalCache {
    fn new(capacity: usize) -> Self {
        Self {
            ids: HashSet::new(),
            order: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn contains(&self, id: ProposalId) -> bool {
        self.ids.contains(&id)
    }

    fn record(&mut self, id: ProposalId) {
        if !self.ids.insert(id) {
            return;
        }
        self.order.push_back(id);
        while self.order.len() > self.capacity
            && let Some(old) = self.order.pop_front()
        {
            self.ids.remove(&old);
        }
    }
}

/// Test-only stubs shared across `core/`'s unit-test modules.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::de_mls::messages::v1::{MemberInvite, RemoveMember};

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    fn insert_self_leave(conversation: &mut Conversation, identity: &[u8]) {
        let remove = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::RemoveMember(
                RemoveMember {
                    identity: identity.to_vec(),
                },
            )),
        };
        conversation.insert_approved_proposal(self_leave_proposal_id(identity), remove);
    }

    #[test]
    fn test_prune_clears_self_leave_entry_when_member_gone() {
        let mut conversation = Conversation::new("test-conversation");

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
        let mut cache = ResolvedProposalCache::new(3);
        cache.record(1);
        cache.record(2);
        cache.record(3);
        assert!(cache.contains(1));
        assert!(cache.contains(2));
        assert!(cache.contains(3));

        cache.record(4);
        assert!(!cache.contains(1), "oldest entry must be evicted");
        assert!(cache.contains(2));
        assert!(cache.contains(3));
        assert!(cache.contains(4));
    }

    #[test]
    fn resolved_cache_dedupes_and_does_not_bump_position() {
        let mut cache = ResolvedProposalCache::new(3);
        cache.record(1);
        cache.record(2);
        cache.record(1); // no-op: 1 stays in its original slot
        cache.record(3);
        assert!(cache.contains(1));
        assert!(cache.contains(2));
        assert!(cache.contains(3));

        // Fourth distinct id evicts the oldest (1), not a later entry.
        cache.record(4);
        assert!(!cache.contains(1));
        assert!(cache.contains(2));
        assert!(cache.contains(3));
        assert!(cache.contains(4));
    }

    #[test]
    fn mark_consensus_outcome_persists_in_resolved_cache() {
        let mut conversation = Conversation::new("g");
        assert!(!conversation.is_consensus_outcome_applied(42));
        conversation.mark_consensus_outcome_applied(42);
        assert!(conversation.is_consensus_outcome_applied(42));
    }

    fn insert_remove_member(
        conversation: &mut Conversation,
        target: &[u8],
        proposal_id: ProposalId,
    ) {
        let remove = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::RemoveMember(
                RemoveMember {
                    identity: target.to_vec(),
                },
            )),
        };
        conversation.insert_approved_proposal(proposal_id, remove);
    }

    /// `RemoveMember` proposals survive a freeze failure regardless of
    /// source; Add proposals are dropped.
    #[test]
    fn test_reject_all_approved_preserves_all_remove_member() {
        let mut conversation = Conversation::new("test-conversation");

        let ban_id: ProposalId = 0x1111_2222;
        let ecp_id: ProposalId = 0x3333_4444;
        let add_id: ProposalId = 0x5555_6666;
        insert_remove_member(&mut conversation, &member(2), ban_id);
        insert_remove_member(&mut conversation, &member(3), ecp_id);
        let add = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::MemberInvite(
                MemberInvite {
                    key_package_bytes: vec![0; 8],
                    identity: member(99),
                },
            )),
        };
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
    fn test_approved_order_preserves_fifo_across_mutations() {
        let mut conversation = Conversation::new("g");

        insert_remove_member(&mut conversation, &member(2), 500);
        insert_remove_member(&mut conversation, &member(3), 100);
        insert_remove_member(&mut conversation, &member(4), 300);
        assert_eq!(conversation.approved_order(), &[500, 100, 300]);

        conversation.remove_approved_proposal(100);
        assert_eq!(conversation.approved_order(), &[500, 300]);

        // Re-inserting an existing id does not duplicate or reorder.
        insert_remove_member(&mut conversation, &member(2), 500);
        assert_eq!(conversation.approved_order(), &[500, 300]);

        conversation.clear_approved_proposals();
        assert!(conversation.approved_order().is_empty());
    }

    #[test]
    fn test_urgent_commit_target_set_take_clears() {
        let mut conversation = Conversation::new("g");
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
        let mut conversation = Conversation::new("g");
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

    fn buffer_remove_at(conversation: &mut Conversation, target: &[u8], epoch: u64) {
        let request = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::RemoveMember(
                RemoveMember {
                    identity: target.to_vec(),
                },
            )),
        };
        assert!(conversation.buffer_pending_update(request, epoch));
    }

    /// Reducing `pending_update_max_epochs` (e.g. via a tightened
    /// `ConversationSync`) must expire entries whose age now exceeds the new max.
    /// Cutoff math: `current_epoch - max_age`; entries with
    /// `first_seen_epoch < cutoff` are dropped.
    #[test]
    fn test_expire_pending_updates_drops_entries_older_than_max_age() {
        let mut conversation = Conversation::new("g");
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
        let mut conversation = Conversation::new("g");
        let prior = member(7);
        let current = member(9);

        buffer_remove_at(&mut conversation, &prior, 4);
        buffer_remove_at(&mut conversation, &current, 5);

        let expired = conversation.expire_pending_updates(5, 0);

        assert_eq!(expired, vec![prior.clone()]);
        assert!(conversation.has_pending_update(&current));
        assert!(!conversation.has_pending_update(&prior));
    }

    /// `reject_all_voting_proposals` empties the owner-side voting queue —
    /// proposals the local node submitted but never reached consensus
    /// must not survive into the next round.
    #[test]
    fn reject_all_voting_proposals_empties_owner_queue() {
        let mut conversation = Conversation::new("reject-voting");
        conversation.store_voting_proposal(1, ConversationUpdateRequest { payload: None });
        conversation.store_voting_proposal(2, ConversationUpdateRequest { payload: None });
        assert!(conversation.is_owner_of_proposal(1));
        assert!(conversation.is_owner_of_proposal(2));

        conversation.reject_all_voting_proposals();

        assert!(!conversation.is_owner_of_proposal(1));
        assert!(!conversation.is_owner_of_proposal(2));
    }

    /// `observe → has → resolve` cycle for `pending_removal_targets`,
    /// covering the idempotent re-observe path.
    #[test]
    fn pending_removal_target_observe_resolve_cycle() {
        let mut conversation = Conversation::new("dedup");
        let target = member(10);

        assert!(!conversation.has_pending_removal(&target));

        conversation.observe_pending_removal(target.clone());
        assert!(conversation.has_pending_removal(&target));

        conversation.observe_pending_removal(target.clone());
        assert!(
            conversation.has_pending_removal(&target),
            "second observe is idempotent"
        );

        conversation.resolve_pending_removal(&target);
        assert!(!conversation.has_pending_removal(&target));
    }

    /// Once an ECP score-below-threshold YES has resolved into a queued
    /// `RemoveMember`, `is_pending_removal` flips true (it's in the approved
    /// queue) and `has_pending_removal` flips false (the in-flight ECP dedup
    /// is cleared). Both gates together must keep the steward from
    /// re-proposing for the same target.
    #[test]
    fn below_threshold_target_queued_for_removal_is_not_re_proposed() {
        use crate::core::apply_consensus_result;
        use crate::protos::de_mls::messages::v1::ViolationEvidence;
        use prost::Message;

        let mut conversation = Conversation::new("removal-no-duplicate");
        let creator = member(1);
        let target = member(7);

        let evidence =
            ViolationEvidence::score_below_threshold(target.clone(), 0, 0).with_creator(creator);
        let request = evidence.into_update_request().unwrap();
        let payload = request.encode_to_vec();
        let proposal_id = 300;
        conversation.store_voting_proposal(proposal_id, request);
        conversation.observe_pending_removal(target.clone());

        apply_consensus_result(&mut conversation, proposal_id, true, &payload).unwrap();
        // Mirror the coordinator: clear the in-flight ECP dedup on resolution.
        conversation.resolve_pending_removal(&target);

        assert!(
            conversation.is_pending_removal(&target),
            "RemoveMember should be queued in approved_proposals"
        );
        assert!(
            !conversation.has_pending_removal(&target),
            "in-flight ECP dedup is cleared once the ECP resolves"
        );
    }
}
