//! Per-group protocol-queue state: approved/voting proposal queues,
//! freeze-round candidate buffer, pending-update buffer, urgent-commit
//! target, recovery-mode flag, ECP dedup. MLS crypto state and the
//! steward-list plug-in live alongside on `GroupEntry`.

use std::collections::{HashMap, HashSet, VecDeque};

use sha2::{Digest, Sha256};

use crate::{
    core::proposal_kind::ProposalKind,
    protos::de_mls::messages::v1::{CommitCandidate, GroupUpdateRequest, group_update_request},
};

/// Consensus proposal identifier (assigned by the consensus service).
pub type ProposalId = u32;

/// Deterministic proposal ID for a self-leave, derived from the leaver's
/// identity. Pinning the ID is what makes a leaver's crash-retry dedupe
/// against an in-flight session (`ProposalAlreadyExist`) instead of opening
/// a second session that would land a duplicate `RemoveMember` in the next
/// commit batch. It also doubles as the self-leave signature used by
/// `is_auto_approved_entry` and `reject_all_approved_proposals`.
pub fn auto_approved_leave_proposal_id(identity: &[u8]) -> u32 {
    let hash = Sha256::digest(identity);
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
}

/// True iff the `(proposal_id, request)` pair is an auto-approved self-leave
/// (identified by the deterministic ID signature).
pub fn is_auto_approved_entry(proposal_id: u32, request: &GroupUpdateRequest) -> bool {
    match request.payload.as_ref() {
        Some(group_update_request::Payload::RemoveMember(r)) => {
            proposal_id == auto_approved_leave_proposal_id(&r.identity)
        }
        _ => false,
    }
}

/// Borrow-only `HashSet` view over a slice of identity blobs, for O(1)
/// membership lookups against `Vec<Vec<u8>>`.
pub fn member_set(members: &[Vec<u8>]) -> HashSet<&[u8]> {
    members.iter().map(|m| m.as_slice()).collect()
}

/// Return the target identity of a membership-changing `GroupUpdateRequest`.
///
/// Used as the stable key for buffering pending updates so duplicates don't
/// stack when the same KP is re-broadcast. Returns `None` for non-membership
/// requests (emergency criteria, steward election).
pub fn target_identity_of(request: &GroupUpdateRequest) -> Option<&[u8]> {
    match request.payload.as_ref()? {
        group_update_request::Payload::InviteMember(m) => Some(&m.identity),
        group_update_request::Payload::RemoveMember(m) => Some(&m.identity),
        _ => None,
    }
}

/// A membership update that has been observed but may not yet have been committed.
///
/// Every member buffers these so that, if the epoch steward fails to commit the
/// change, the next epoch steward can pick it up. Entries are pruned once the
/// change has been applied to the group (member added/removed) or after
/// `max_age_epochs` epochs have elapsed since the entry was first seen.
#[derive(Clone, Debug)]
pub struct PendingUpdate {
    /// The `GroupUpdateRequest` carrying either `InviteMember` or `RemoveMember`.
    pub request: GroupUpdateRequest,
    /// MLS epoch at which this update was first observed locally.
    pub first_seen_epoch: u64,
}

const MAX_COMMITTED_HASHES: usize = 10;

/// A commit candidate buffered during freeze for later selection.
#[derive(Clone, Debug)]
pub struct BufferedCommitCandidate {
    pub candidate_msg: CommitCandidate,
    pub commit_hash: Vec<u8>,
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

/// Per-group protocol state. Stewards batch commits; members vote.
/// Construct with [`Self::create_group`] or [`Self::prepare_to_join`].
///
/// Pure proposal-queue and freeze-round bookkeeping — MLS state lives
/// on the per-group [`crate::mls_crypto::MlsService`] instance held on
/// `GroupEntry`, alongside the steward-list plug-in. `Group` itself has
/// no `M` generic.
pub struct Group {
    /// The name of the group.
    group_name: String,
    /// This user's identity bytes (set at construction; matches the
    /// MLS leaf credential).
    self_identity: Vec<u8>,
    /// Proposals that passed consensus, waiting for steward to commit.
    approved_proposals: HashMap<ProposalId, GroupUpdateRequest>,
    /// Insertion order of `approved_proposals` (FIFO). Library proposal
    /// IDs are content-derived hashes, so sort-by-id is not temporal.
    approved_order: Vec<ProposalId>,
    /// Proposals waiting on consensus voting (created by this user).
    voting_proposals: HashMap<ProposalId, GroupUpdateRequest>,
    /// Active emergency criteria proposals not yet finalized by consensus.
    /// While non-empty, lower-priority proposals MUST be blocked (RFC §Partial Freeze).
    active_emergency_ids: HashSet<ProposalId>,
    /// Members with pending score-based removal ECPs (dedup to prevent duplicates).
    pending_removal_targets: HashSet<Vec<u8>>,
    /// Recent commit hashes for dedup.
    committed_batch_hashes: VecDeque<Vec<u8>>,
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
    /// While `true`, `create_commit_candidate` bypasses the steward gate
    /// so any member can produce the recovery commit. Set by Layer-3
    /// Deadlock ECP, cleared on accepted election.
    recovery_mode: bool,
}

pub const DEFAULT_LIVENESS_CRITERIA_YES: bool = true;

pub const DEFAULT_PENDING_UPDATE_MAX_EPOCHS: u32 = 3;

impl Group {
    fn new_base(group_name: &str, self_identity: Vec<u8>) -> Self {
        Self {
            group_name: group_name.to_string(),
            self_identity,
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
            recovery_mode: false,
        }
    }

    /// Local identity bytes for this group's member (this user's wallet
    /// address). Set at construction; same value MLS will surface on this
    /// group's leaf credential.
    pub fn self_identity(&self) -> &[u8] {
        &self.self_identity
    }

    pub(crate) fn is_consensus_outcome_applied(&self, proposal_id: ProposalId) -> bool {
        self.resolved_proposals.contains(proposal_id)
    }

    pub(crate) fn mark_consensus_outcome_applied(&mut self, proposal_id: ProposalId) {
        self.resolved_proposals.record(proposal_id);
    }

    /// Build a joiner-side handle for an existing group. The MLS service
    /// is held on `GroupEntry`; the lifecycle layer attaches it via
    /// `entry.attach_mls(...)` once the welcome arrives.
    pub fn prepare_to_join(group_name: &str, self_identity: Vec<u8>) -> Self {
        Self::new_base(group_name, self_identity)
    }

    /// Build a creator-side handle. The steward list and MLS service
    /// are owned by `GroupEntry`; this constructor only initializes the
    /// proposal-queue and freeze-round state.
    pub fn create_group(group_name: &str, creator_identity: Vec<u8>) -> Self {
        Self::new_base(group_name, creator_identity)
    }

    pub fn group_name(&self) -> &str {
        &self.group_name
    }

    pub fn group_name_bytes(&self) -> &[u8] {
        self.group_name.as_bytes()
    }

    /// Build the eligibility predicate that the steward plug-in's "live"
    /// position queries take. A candidate is eligible when they are an
    /// MLS member of the group AND don't have a removal queued in
    /// `approved_proposals`. The closure borrows from `self` and
    /// `mls_members` for the lifetime of the call.
    pub fn steward_eligibility<'a>(
        &'a self,
        mls_members: &'a [Vec<u8>],
    ) -> impl Fn(&[u8]) -> bool + 'a {
        let mls_set = member_set(mls_members);
        move |candidate: &[u8]| !self.is_pending_removal(candidate) && mls_set.contains(candidate)
    }

    /// True iff `approved_proposals` carries any `RemoveMember(identity)` —
    /// regardless of source (self-leave, ban, ECP-derived). Used by the
    /// steward-eligibility predicate to skip a member whose removal is queued
    /// for the next commit: MLS forbids them from committing it themselves,
    /// so the rotation walk would have to fall back anyway. Broader than
    /// [`Self::is_pending_self_leave`] (which only matches the deterministic
    /// self-leave id).
    pub fn is_pending_removal(&self, identity: &[u8]) -> bool {
        self.approved_proposals.values().any(|req| {
            matches!(
                req.payload.as_ref(),
                Some(group_update_request::Payload::RemoveMember(r)) if r.identity == identity
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

    pub fn approved_proposals(&self) -> &HashMap<ProposalId, GroupUpdateRequest> {
        &self.approved_proposals
    }

    /// Insertion order of `approved_proposals` (oldest first).
    pub fn approved_order(&self) -> &[ProposalId] {
        &self.approved_order
    }

    /// Re-inserting an existing id preserves the original position.
    fn push_approved(&mut self, proposal_id: ProposalId, proposal: GroupUpdateRequest) {
        if self
            .approved_proposals
            .insert(proposal_id, proposal)
            .is_none()
        {
            self.approved_order.push(proposal_id);
        }
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
    pub fn store_voting_proposal(&mut self, proposal_id: ProposalId, proposal: GroupUpdateRequest) {
        self.voting_proposals.insert(proposal_id, proposal);
    }

    /// Insert a proposal straight into the approved queue (non-owner path).
    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: GroupUpdateRequest,
    ) {
        self.push_approved(proposal_id, proposal);
    }

    /// Drop a single proposal from the approved queue without archiving.
    pub fn remove_approved_proposal(&mut self, proposal_id: ProposalId) {
        if self.approved_proposals.remove(&proposal_id).is_some() {
            self.approved_order.retain(|pid| *pid != proposal_id);
        }
    }

    /// Clear the approved-proposal queue and return the cleared batch so
    /// callers can archive it for UI / diagnostic history. Returns an
    /// empty `HashMap` when the queue was already empty.
    ///
    /// MLS advances the epoch itself on commit merge, so no counter bump
    /// here. Per-node epoch history is an app-layer concern; this method
    /// surfaces the snapshot but does not retain it.
    pub fn clear_approved_proposals(&mut self) -> HashMap<ProposalId, GroupUpdateRequest> {
        self.approved_order.clear();
        std::mem::take(&mut self.approved_proposals)
    }

    /// Discard the approved queue on freeze failure. `RemoveMember`
    /// proposals carry settled YES outcomes and survive so a recovered
    /// steward can commit them; other approvals (Add) are dropped.
    pub fn reject_all_approved_proposals(&mut self) {
        self.approved_proposals.retain(|_pid, req| {
            matches!(
                req.payload.as_ref(),
                Some(group_update_request::Payload::RemoveMember(_))
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

    /// Mark the next freeze cycle as urgent and committed-only-for `target`.
    pub(crate) fn set_urgent_commit_target(&mut self, target: Vec<u8>) {
        self.urgent_commit_target = Some(target);
    }

    pub fn urgent_commit_target(&self) -> Option<&[u8]> {
        self.urgent_commit_target.as_deref()
    }

    pub(crate) fn take_urgent_commit_target(&mut self) -> Option<Vec<u8>> {
        self.urgent_commit_target.take()
    }

    /// Drop every `RemoveMember(target)` entry; other approvals stay
    /// queued for the next normal cycle.
    pub fn drop_approved_removals_for(&mut self, target: &[u8]) {
        self.approved_proposals.retain(|_pid, req| {
            !matches!(
                req.payload.as_ref(),
                Some(group_update_request::Payload::RemoveMember(r)) if r.identity == target
            )
        });
        self.approved_order
            .retain(|pid| self.approved_proposals.contains_key(pid));
    }

    // ─────────────────────────── Layer 3 Recovery Mode ───────────────────────────

    /// While set, `create_commit_candidate` bypasses the `is_steward()`
    /// gate so any member can produce the recovery commit.
    pub fn enter_recovery_mode(&mut self) {
        self.recovery_mode = true;
    }

    pub fn is_in_recovery_mode(&self) -> bool {
        self.recovery_mode
    }

    pub fn exit_recovery_mode(&mut self) {
        self.recovery_mode = false;
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

    fn build_freeze_round(&self, epoch: u64) -> FreezeRound {
        FreezeRound {
            epoch,
            selection_locked: false,
            candidates: Vec::new(),
        }
    }

    /// Ensure a freeze round exists for the given MLS epoch.
    ///
    /// If absent or stale, initializes one with the current approved proposal IDs.
    /// The `epoch` parameter should be the current MLS epoch from `OpenMlsService::current_epoch()`.
    pub(crate) fn ensure_freeze_round(&mut self, epoch: u64) {
        if matches!(self.freeze_round, Some(ref round) if round.epoch == epoch) {
            return;
        }
        self.freeze_round = Some(self.build_freeze_round(epoch));
    }

    /// Start a new freeze round for the given MLS epoch.
    ///
    /// Existing round state is replaced.
    pub fn start_freeze_round(&mut self, epoch: u64) {
        self.freeze_round = Some(self.build_freeze_round(epoch));
    }

    /// Add a validated candidate to the active freeze round.
    ///
    /// Returns `true` if buffered, `false` if ignored (locked round or duplicate).
    /// The `epoch` parameter should be the current MLS epoch from `OpenMlsService::current_epoch()`.
    pub fn add_freeze_candidate(&mut self, candidate: BufferedCommitCandidate, epoch: u64) -> bool {
        self.ensure_freeze_round(epoch);
        let Some(round) = self.freeze_round.as_mut() else {
            return false;
        };

        if round.epoch != epoch || round.selection_locked {
            return false;
        }

        if round
            .candidates
            .iter()
            .any(|c| c.commit_hash == candidate.commit_hash)
        {
            return false;
        }

        round.candidates.push(candidate);
        true
    }

    /// Mark the active freeze round as selection-locked.
    /// The `epoch` parameter should be the current MLS epoch from `OpenMlsService::current_epoch()`.
    pub(crate) fn lock_freeze_round_selection(&mut self, epoch: u64) {
        if let Some(round) = self.freeze_round.as_mut() {
            if round.epoch == epoch {
                round.selection_locked = true;
            }
        }
    }

    /// Read-only access to the active freeze round.
    pub(crate) fn freeze_round(&self) -> Option<&FreezeRound> {
        self.freeze_round.as_ref()
    }

    /// Get the number of buffered commit candidates in the active freeze round.
    pub fn freeze_candidate_count(&self) -> usize {
        self.freeze_round
            .as_ref()
            .map(|r| r.candidates.len())
            .unwrap_or(0)
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

    // ─────────────────────────── Dedup Operations ───────────────────────────

    /// Check if a commit hash has already been committed (in committed history).
    ///
    /// Note: freeze round buffer dedup is handled separately by `add_freeze_candidate`.
    pub(crate) fn is_duplicate_commit_candidate(&self, commit_hash: &[u8]) -> bool {
        self.committed_batch_hashes
            .iter()
            .any(|ch| ch == commit_hash)
    }

    /// Record a committed batch's hash for future dedup.
    pub(crate) fn record_committed_batch(&mut self, commit_hash: Vec<u8>) {
        if self.committed_batch_hashes.len() >= MAX_COMMITTED_HASHES {
            self.committed_batch_hashes.pop_front();
        }
        self.committed_batch_hashes.push_back(commit_hash);
    }

    // ─────────────────────────── Pending Update Buffer ───────────────────────────

    /// Insert a `GroupUpdateRequest` into the pending-updates buffer.
    ///
    /// Keyed by target identity — a second insertion for the same identity
    /// keeps the original `first_seen_epoch` so it can still expire on schedule.
    /// Returns `true` if this is a new entry, `false` if already buffered.
    pub fn buffer_pending_update(
        &mut self,
        request: GroupUpdateRequest,
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
        let pid = auto_approved_leave_proposal_id(identity);
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

    /// Drop Add entries whose target is now a group member, and Remove entries
    /// whose target is no longer a group member. Call after a commit has merged.
    ///
    /// Also drops any auto-approved self-leave in `approved_proposals` whose
    /// target is no longer a group member (the leave has been applied).
    /// Regular approved entries are normally drained by
    /// `clear_approved_proposals` on the same commit path; this is an extra
    /// sweep to catch auto-approved entries that survived freeze failures.
    pub fn prune_pending_updates_for_members(&mut self, current_members: &[Vec<u8>]) {
        let in_group: HashSet<&Vec<u8>> = current_members.iter().collect();
        self.pending_updates.retain(|identity, entry| {
            let payload = match entry.request.payload.as_ref() {
                Some(p) => p,
                None => return false,
            };
            match payload {
                group_update_request::Payload::InviteMember(_) => !in_group.contains(identity),
                group_update_request::Payload::RemoveMember(_) => in_group.contains(identity),
                _ => false,
            }
        });

        // Sweep orphaned auto-approved leaves (target no longer in group).
        self.approved_proposals.retain(|pid, req| {
            if !is_auto_approved_entry(*pid, req) {
                return true;
            }
            match req.payload.as_ref() {
                Some(group_update_request::Payload::RemoveMember(r)) => {
                    in_group.contains(&r.identity)
                }
                _ => true,
            }
        });
        self.approved_order
            .retain(|pid| self.approved_proposals.contains_key(pid));
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
    use crate::protos::de_mls::messages::v1::{InviteMember, RemoveMember};

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    /// Test convenience: build a creator-side `Group`. Steward
    /// list is owned by the per-group plug-in (covered separately in
    /// `core::steward_list_plugin::tests`); these tests focus on
    /// proposal-queue + dedup + freeze-round logic on `Group`.
    fn creator_group(name: &str, identity: Vec<u8>) -> Group {
        Group::create_group(name, identity)
    }

    fn insert_self_leave(group: &mut Group, identity: &[u8]) {
        let remove = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: identity.to_vec(),
            })),
        };
        group.insert_approved_proposal(auto_approved_leave_proposal_id(identity), remove);
    }

    #[test]
    fn test_reject_all_approved_preserves_self_leave_entry() {
        let mut group = creator_group("test-group", member(1));

        let leaver = member(2);
        insert_self_leave(&mut group, &leaver);
        let ban_id: ProposalId = 0xdead_beef;
        let ban = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                key_package_bytes: vec![0; 8],
                identity: member(99),
            })),
        };
        group.insert_approved_proposal(ban_id, ban);
        assert_eq!(group.approved_proposals_count(), 2);

        // Simulate freeze failure.
        group.reject_all_approved_proposals();

        // Self-leave entry (deterministic id) survives; unrelated approvals drop.
        assert_eq!(group.approved_proposals_count(), 1);
        let leave_id = auto_approved_leave_proposal_id(&leaver);
        assert!(group.approved_proposals().contains_key(&leave_id));
        assert!(!group.approved_proposals().contains_key(&ban_id));
    }

    #[test]
    fn test_prune_clears_self_leave_entry_when_member_gone() {
        let mut group = creator_group("test-group", member(1));

        let leaver = member(2);
        insert_self_leave(&mut group, &leaver);
        assert_eq!(group.approved_proposals_count(), 1);
        assert!(group.is_pending_self_leave(&leaver));

        // Commit merged — leaver is no longer a member.
        let after = members(&[1, 3]);
        group.prune_pending_updates_for_members(&after);

        assert_eq!(group.approved_proposals_count(), 0);
        assert!(!group.is_pending_self_leave(&leaver));
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
        let mut group = creator_group("g", member(1));
        assert!(!group.is_consensus_outcome_applied(42));
        group.mark_consensus_outcome_applied(42);
        assert!(group.is_consensus_outcome_applied(42));
    }

    fn insert_remove_member(group: &mut Group, target: &[u8], proposal_id: ProposalId) {
        let remove = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: target.to_vec(),
            })),
        };
        group.insert_approved_proposal(proposal_id, remove);
    }

    /// `RemoveMember` proposals survive a freeze failure regardless of
    /// source; Add proposals are dropped.
    #[test]
    fn test_reject_all_approved_preserves_all_remove_member() {
        let mut group = creator_group("test-group", member(1));

        let ban_id: ProposalId = 0x1111_2222;
        let ecp_id: ProposalId = 0x3333_4444;
        let add_id: ProposalId = 0x5555_6666;
        insert_remove_member(&mut group, &member(2), ban_id);
        insert_remove_member(&mut group, &member(3), ecp_id);
        let add = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                key_package_bytes: vec![0; 8],
                identity: member(99),
            })),
        };
        group.insert_approved_proposal(add_id, add);
        assert_eq!(group.approved_proposals_count(), 3);

        group.reject_all_approved_proposals();

        assert_eq!(group.approved_proposals_count(), 2);
        assert!(group.approved_proposals().contains_key(&ban_id));
        assert!(group.approved_proposals().contains_key(&ecp_id));
        assert!(!group.approved_proposals().contains_key(&add_id));
    }

    /// `approved_order` is FIFO regardless of proposal-id ordering.
    #[test]
    fn test_approved_order_preserves_fifo_across_mutations() {
        let mut group = creator_group("g", member(1));

        insert_remove_member(&mut group, &member(2), 500);
        insert_remove_member(&mut group, &member(3), 100);
        insert_remove_member(&mut group, &member(4), 300);
        assert_eq!(group.approved_order(), &[500, 100, 300]);

        group.remove_approved_proposal(100);
        assert_eq!(group.approved_order(), &[500, 300]);

        // Re-inserting an existing id does not duplicate or reorder.
        insert_remove_member(&mut group, &member(2), 500);
        assert_eq!(group.approved_order(), &[500, 300]);

        group.clear_approved_proposals();
        assert!(group.approved_order().is_empty());
    }

    #[test]
    fn test_urgent_commit_target_set_take_clears() {
        let mut group = creator_group("g", member(1));
        assert!(group.urgent_commit_target().is_none());

        let target = member(7);
        group.set_urgent_commit_target(target.clone());
        assert_eq!(group.urgent_commit_target(), Some(target.as_slice()));

        let taken = group.take_urgent_commit_target().unwrap();
        assert_eq!(taken, target);
        assert!(group.urgent_commit_target().is_none());
    }

    #[test]
    fn test_drop_approved_removals_for_target() {
        let mut group = creator_group("g", member(1));
        let victim = member(7);
        let bystander = member(9);

        insert_remove_member(&mut group, &victim, 100);
        insert_remove_member(&mut group, &victim, 101);
        insert_remove_member(&mut group, &bystander, 200);
        assert_eq!(group.approved_proposals_count(), 3);

        group.drop_approved_removals_for(&victim);

        assert_eq!(group.approved_proposals_count(), 1);
        assert!(group.approved_proposals().contains_key(&200));
        assert!(!group.approved_proposals().contains_key(&100));
        assert!(!group.approved_proposals().contains_key(&101));
    }

    fn buffer_remove_at(group: &mut Group, target: &[u8], epoch: u64) {
        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: target.to_vec(),
            })),
        };
        assert!(group.buffer_pending_update(request, epoch));
    }

    /// Reducing `pending_update_max_epochs` (e.g. via a tightened
    /// `GroupSync`) must expire entries whose age now exceeds the new max.
    /// Cutoff math: `current_epoch - max_age`; entries with
    /// `first_seen_epoch < cutoff` are dropped.
    #[test]
    fn test_expire_pending_updates_drops_entries_older_than_max_age() {
        let mut group = creator_group("g", member(1));
        let stale = member(7);
        let fresh = member(9);

        buffer_remove_at(&mut group, &stale, 0);
        buffer_remove_at(&mut group, &fresh, 4);
        assert_eq!(group.pending_update_count(), 2);

        let expired = group.expire_pending_updates(5, 1);

        assert_eq!(expired, vec![stale.clone()]);
        assert_eq!(group.pending_update_count(), 1);
        assert!(group.has_pending_update(&fresh));
        assert!(!group.has_pending_update(&stale));
    }

    /// `max_age = 0` keeps only entries from the current epoch — the
    /// boundary case a tightened sync hits when shrinking the window.
    #[test]
    fn test_expire_pending_updates_max_age_zero_keeps_only_current_epoch() {
        let mut group = creator_group("g", member(1));
        let prior = member(7);
        let current = member(9);

        buffer_remove_at(&mut group, &prior, 4);
        buffer_remove_at(&mut group, &current, 5);

        let expired = group.expire_pending_updates(5, 0);

        assert_eq!(expired, vec![prior.clone()]);
        assert!(group.has_pending_update(&current));
        assert!(!group.has_pending_update(&prior));
    }
}
