//! Per-group app-level state: proposal queues, steward list, freeze-round
//! candidate buffer, pending-update buffer, ECP dedup. MLS crypto state
//! lives in `MlsService` alongside this.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::core::CoreError;
use crate::core::proposal_kind::ProposalKind;
use crate::core::steward_list::{ProtocolConfig, StewardList};
use crate::protos::de_mls::messages::v1::{
    CommitCandidate, GroupUpdateRequest, group_update_request,
};

/// Consensus proposal identifier (assigned by the consensus service).
pub type ProposalId = u32;

/// How many past approved-proposal batches to retain for UI display.
///
/// RFC §"Creating Voting Proposal" requires retaining finalized proposals for
/// at least `threshold_duration`; this count-based cap is a placeholder until
/// that becomes a first-class config value (see `docs/ROADMAP.md`).
const MAX_EPOCH_HISTORY: usize = 10;

/// Derive a deterministic proposal ID for an auto-approved self-leave,
/// keyed on the leaver's identity. Every node computes the same ID, so
/// `approved_proposals` stays consistent across the group without
/// running a consensus round.
///
/// This ID doubles as the "auto-approved" signature — an approved entry
/// whose ID matches this formula for its own target is known to be a
/// self-leave (not a consensus-approved Remove).
pub fn auto_approved_leave_proposal_id(identity: &[u8]) -> u32 {
    use sha2::{Digest, Sha256};
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
pub(crate) struct BufferedCommitCandidate {
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

/// Per-group app-level state. Wrap in `RwLock` at the app layer — DE-MLS
/// holds this across async contexts. Stewards batch commits; members vote.
/// Construct with [`Self::new_as_creator`] or [`Self::new_as_joiner`].
#[derive(Clone, Debug)]
pub struct Group {
    /// The name of the group.
    group_name: String,
    /// This user's wallet identity (for deriving steward status from the list).
    self_identity: Vec<u8>,
    /// Proposals that passed consensus, waiting for steward to commit.
    approved_proposals: HashMap<ProposalId, GroupUpdateRequest>,
    /// Proposals waiting on consensus voting (created by this user).
    voting_proposals: HashMap<ProposalId, GroupUpdateRequest>,
    /// History of previously-committed batches (most recent last), capped
    /// at [`MAX_EPOCH_HISTORY`] entries.
    epoch_history: VecDeque<HashMap<ProposalId, GroupUpdateRequest>>,
    /// Active steward list for the current epoch window.
    /// `Some` after bootstrap (creator) or sync (joiner). `None` only for joiners pre-sync.
    steward_list: Option<StewardList>,
    /// Protocol configuration (steward list bounds and protocol-level flags).
    protocol_config: ProtocolConfig,
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
    /// Current steward-election retry round. Starts at 0, increments
    /// every time an election proposal is rejected within the same MLS
    /// epoch, reset to 0 when an election is accepted. Threaded into
    /// [`StewardList::generate`] so each retry proposes a different
    /// list composition.
    reelection_round: u32,
    /// Group-configured ceiling on steward-election retries before the
    /// "stuck" error surfaces. Every member in the group holds the same
    /// value — joiners pick it up from `GroupSync` — so the error path
    /// triggers consistently across the group. Overridden at group
    /// creation from `GroupConfig`; defaults to [`DEFAULT_MAX_REELECTION_RETRIES`].
    max_reelection_retries: u32,
    /// Proposal IDs already dispatched through `apply_consensus_outcome`.
    /// The consensus library can re-emit `ConsensusReached` (timeout-path
    /// race); this guards against re-applying state and double-firing events.
    consensus_outcomes_applied: HashSet<ProposalId>,
}

/// Fallback ceiling on steward-election retries. One retry gives the
/// responsible proposer a second shot with a different list composition;
/// beyond that human/policy intervention is expected.
pub const DEFAULT_MAX_REELECTION_RETRIES: u32 = 1;

impl Group {
    fn new_base(group_name: &str, self_identity: Vec<u8>, protocol_config: ProtocolConfig) -> Self {
        Self {
            group_name: group_name.to_string(),
            self_identity,
            approved_proposals: HashMap::new(),
            voting_proposals: HashMap::new(),
            epoch_history: VecDeque::new(),
            steward_list: None,
            protocol_config,
            active_emergency_ids: HashSet::new(),
            pending_removal_targets: HashSet::new(),
            committed_batch_hashes: VecDeque::new(),
            freeze_round: None,
            pending_updates: HashMap::new(),
            reelection_round: 0,
            max_reelection_retries: DEFAULT_MAX_REELECTION_RETRIES,
            consensus_outcomes_applied: HashSet::new(),
        }
    }

    pub fn is_consensus_outcome_applied(&self, proposal_id: ProposalId) -> bool {
        self.consensus_outcomes_applied.contains(&proposal_id)
    }

    pub fn mark_consensus_outcome_applied(&mut self, proposal_id: ProposalId) {
        self.consensus_outcomes_applied.insert(proposal_id);
    }

    /// Create a new group handle for a joining member (not yet steward).
    pub fn new_as_joiner(
        group_name: &str,
        self_identity: Vec<u8>,
        protocol_config: ProtocolConfig,
    ) -> Self {
        Self::new_base(group_name, self_identity, protocol_config)
    }

    /// Create a new group handle for the group creator, initializing the steward list.
    pub fn new_as_creator(
        group_name: &str,
        creator_identity: Vec<u8>,
        protocol_config: ProtocolConfig,
    ) -> Result<Self, CoreError> {
        let mut group = Self::new_base(
            group_name,
            creator_identity.clone(),
            protocol_config.clone(),
        );
        let list = StewardList::generate(
            0,
            group_name.as_bytes(),
            &[creator_identity],
            1,
            protocol_config,
            0, // initial list — no election, no retries
        )?;
        group.steward_list = Some(list);

        Ok(group)
    }

    pub fn group_name(&self) -> &str {
        &self.group_name
    }

    pub fn group_name_bytes(&self) -> &[u8] {
        self.group_name.as_bytes()
    }

    pub fn allow_subset_candidates(&self) -> bool {
        self.protocol_config.allow_subset_candidates
    }

    /// Overwritten when the handle receives a `GroupSync` from the steward.
    pub fn set_allow_subset_candidates(&mut self, allow: bool) {
        self.protocol_config.allow_subset_candidates = allow;
    }

    /// Derived from `steward_list.contains(self_identity)` — `false` for
    /// joiners that haven't received the list yet.
    pub fn is_steward(&self) -> bool {
        self.steward_list
            .as_ref()
            .is_some_and(|l| l.contains(&self.self_identity))
    }

    pub fn is_epoch_steward(&self, epoch: u64) -> bool {
        self.epoch_steward(epoch)
            .is_some_and(|es| es == self.self_identity)
    }

    /// Like [`Self::is_epoch_steward`] but via [`Self::live_epoch_steward`]
    /// (skips members no longer in the group or pending a self-leave).
    pub fn is_live_epoch_steward(&self, epoch: u64, members: &[Vec<u8>]) -> bool {
        self.live_epoch_steward(epoch, members)
            .is_some_and(|es| es == self.self_identity)
    }

    /// Resolve the live epoch steward. Skips stewards no longer in the group
    /// and stewards that have buffered an auto-approved self-leave.
    pub fn live_epoch_steward<'a>(&'a self, epoch: u64, members: &[Vec<u8>]) -> Option<&'a [u8]> {
        self.steward_list.as_ref().and_then(|l| {
            l.live_epoch_steward(epoch, |candidate| {
                self.is_steward_eligible(candidate, members)
            })
        })
    }

    /// Epoch + backup stewards, filtered by steward-eligibility and
    /// guaranteed distinct when ≥2 are eligible.
    pub fn live_epoch_and_backup<'a>(
        &'a self,
        epoch: u64,
        members: &[Vec<u8>],
    ) -> (Option<&'a [u8]>, Option<&'a [u8]>) {
        match self.steward_list.as_ref() {
            Some(l) => l.live_epoch_and_backup(epoch, |c| self.is_steward_eligible(c, members)),
            None => (None, None),
        }
    }

    fn is_steward_eligible(&self, candidate: &[u8], members: &[Vec<u8>]) -> bool {
        !self.is_pending_self_leave(candidate) && members.iter().any(|m| m == candidate)
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

    /// Move a proposal from the voting queue into the approved queue.
    pub fn mark_proposal_as_approved(&mut self, proposal_id: ProposalId) {
        if let Some(proposal) = self.voting_proposals.remove(&proposal_id) {
            self.approved_proposals.insert(proposal_id, proposal);
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
        self.approved_proposals.insert(proposal_id, proposal);
    }

    /// Drop a single proposal from the approved queue without archiving.
    pub fn remove_approved_proposal(&mut self, proposal_id: ProposalId) {
        self.approved_proposals.remove(&proposal_id);
    }

    /// Archive the approved batch to epoch history and clear the queue.
    ///
    /// MLS advances the epoch itself on commit merge, so no counter bump here.
    pub fn clear_approved_proposals(&mut self) {
        if self.approved_proposals.is_empty() {
            return;
        }
        let snapshot = std::mem::take(&mut self.approved_proposals);
        if self.epoch_history.len() >= MAX_EPOCH_HISTORY {
            self.epoch_history.pop_front();
        }
        self.epoch_history.push_back(snapshot);
    }

    /// Discard the approved queue on freeze failure. Auto-approved self-leaves
    /// are preserved — they have a known YES outcome and must survive so the
    /// next epoch steward can commit them.
    pub fn reject_all_approved_proposals(&mut self) {
        self.approved_proposals
            .retain(|pid, req| is_auto_approved_entry(*pid, req));
    }

    /// Drop every entry in the voting queue.
    pub fn reject_all_voting_proposals(&mut self) {
        self.voting_proposals.clear();
    }

    /// Past committed batches, most recent last.
    pub fn epoch_history(&self) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
        &self.epoch_history
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

    // ─────────────────────────── Steward List Operations ───────────────────────────

    pub fn steward_list(&self) -> Option<&StewardList> {
        self.steward_list.as_ref()
    }

    pub fn protocol_config(&self) -> &ProtocolConfig {
        &self.protocol_config
    }

    pub fn epoch_steward(&self, epoch: u64) -> Option<&[u8]> {
        self.steward_list
            .as_ref()
            .and_then(|l| l.epoch_steward(epoch))
    }

    /// Cheap idempotence check for auto-retry: don't submit a second election
    /// while the previous one is still being voted on.
    pub fn has_election_in_flight(&self) -> bool {
        self.voting_proposals
            .values()
            .any(|req| ProposalKind::of(req).is_steward_election())
    }

    /// Check if the steward list is exhausted at the given epoch.
    pub fn is_steward_list_exhausted(&self, epoch: u64) -> bool {
        self.steward_list
            .as_ref()
            .is_some_and(|l| l.is_exhausted(epoch))
    }

    /// Generate a steward list and set it on the group.
    ///
    /// `retry_round` is the seed fed into the SHA256 sort and stored on the
    /// resulting list as its historical tag. Pass the round from the
    /// accepted election proposal; use 0 for the creator's initial list and
    /// for `sn_min` auto-fills, where no election happened.
    pub fn generate_and_set_steward_list(
        &mut self,
        epoch: u64,
        member_ids: &[Vec<u8>],
        sn: usize,
        retry_round: u32,
    ) -> Result<(), CoreError> {
        let config = self.protocol_config.clone();
        let list = StewardList::generate(
            epoch,
            self.group_name.as_bytes(),
            member_ids,
            sn,
            config,
            retry_round,
        )?;
        self.steward_list = Some(list);
        Ok(())
    }

    /// Check whether a proposed steward list matches what this group would
    /// deterministically generate for the given epoch, member set, and
    /// retry round. App layer calls this before applying an election
    /// result; `apply_consensus_result` can't do it itself because it
    /// has no access to the MLS member list.
    pub fn validate_steward_list_proposal(
        &self,
        proposed_stewards: &[Vec<u8>],
        election_epoch: u64,
        member_ids: &[Vec<u8>],
        retry_round: u32,
    ) -> Result<bool, CoreError> {
        StewardList::validate(
            proposed_stewards,
            election_epoch,
            self.group_name.as_bytes(),
            member_ids,
            &self.protocol_config,
            retry_round,
        )
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
    /// The `epoch` parameter should be the current MLS epoch from `MlsService::current_epoch()`.
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
    /// The `epoch` parameter should be the current MLS epoch from `MlsService::current_epoch()`.
    pub(crate) fn add_freeze_candidate(
        &mut self,
        candidate: BufferedCommitCandidate,
        epoch: u64,
    ) -> bool {
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
    /// The `epoch` parameter should be the current MLS epoch from `MlsService::current_epoch()`.
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
    ///
    /// For auto-approved updates (self-leave) use
    /// [`Self::accept_auto_approved_leave`] instead — those have a known
    /// result and go straight to `approved_proposals`, never through
    /// `pending_updates`.
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

    /// Whether the identity has an auto-approved self-leave currently in
    /// `approved_proposals`. Used by live rotation to skip the leaver.
    pub fn is_pending_self_leave(&self, identity: &[u8]) -> bool {
        let pid = auto_approved_leave_proposal_id(identity);
        self.approved_proposals
            .get(&pid)
            .is_some_and(|req| is_auto_approved_entry(pid, req))
    }

    /// Accept an auto-approved self-leave.
    ///
    /// Inserts `RemoveMember(identity)` directly into `approved_proposals`
    /// with a deterministic proposal ID so every node's approved set agrees
    /// without running a consensus round. The deterministic ID doubles as
    /// the "auto-approved" signature — [`Self::reject_all_approved_proposals`]
    /// uses it to preserve the entry across a freeze failure, and
    /// [`Self::is_pending_self_leave`] uses it for rotation skip.
    ///
    /// Caller must have verified that the originator of the request matches
    /// `identity` (e.g., via the MLS-authenticated sender on the app subtopic).
    ///
    /// Returns `true` if this is a new leave, `false` if already recorded or
    /// a `RemoveMember` for this identity is already pending via another
    /// path (ban, ECP). Dedup prevents duplicate Remove entries from landing
    /// in the same commit batch.
    pub fn accept_auto_approved_leave(&mut self, identity: Vec<u8>) -> bool {
        // Dedup against any existing Remove for this identity — approved
        // queue (ban that already passed consensus), pending_updates (ban or
        // ECP still in flight), or a prior self-leave.
        if self.has_any_remove_for(&identity) {
            return false;
        }
        let remove = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(
                crate::protos::de_mls::messages::v1::RemoveMember {
                    identity: identity.clone(),
                },
            )),
        };
        let proposal_id = auto_approved_leave_proposal_id(&identity);
        self.approved_proposals.insert(proposal_id, remove);
        true
    }

    /// True if any Remove targeting `identity` is in flight — approved queue
    /// (consensus-approved ban or prior self-leave) or pending_updates (ban
    /// still voting, ECP-derived). Used to dedup before an auto-approved
    /// insertion.
    fn has_any_remove_for(&self, identity: &[u8]) -> bool {
        self.has_pending_remove(identity) || self.has_approved_remove(identity)
    }

    fn has_approved_remove(&self, identity: &[u8]) -> bool {
        self.approved_proposals.values().any(|req| {
            matches!(
                req.payload.as_ref(),
                Some(group_update_request::Payload::RemoveMember(r)) if r.identity == identity
            )
        })
    }

    /// Current steward-election retry round (0 for fresh elections).
    pub fn reelection_round(&self) -> u32 {
        self.reelection_round
    }

    /// Increment the retry round. Called on rejected election; the next
    /// election proposal uses the bumped round so its list composition
    /// differs.
    pub fn bump_reelection_round(&mut self) {
        self.reelection_round = self.reelection_round.saturating_add(1);
    }

    /// Reset the retry round. Called on accepted election.
    pub fn reset_reelection_round(&mut self) {
        self.reelection_round = 0;
    }

    /// Group-configured ceiling on retries before the stuck-election error
    /// surfaces. Shared across the group via `GroupSync`.
    pub fn max_reelection_retries(&self) -> u32 {
        self.max_reelection_retries
    }

    /// Overwrite the retry ceiling. Called from group-creation (from
    /// `GroupConfig`) and on joiner sync (from `GroupSync.max_reelection_retries`).
    pub fn set_max_reelection_retries(&mut self, max: u32) {
        self.max_reelection_retries = max;
    }

    fn has_pending_remove(&self, identity: &[u8]) -> bool {
        self.pending_updates
            .get(identity)
            .map(|p| {
                matches!(
                    p.request.payload.as_ref(),
                    Some(group_update_request::Payload::RemoveMember(_))
                )
            })
            .unwrap_or(false)
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    fn default_config() -> ProtocolConfig {
        ProtocolConfig::new(1, 5).unwrap()
    }

    #[test]
    fn test_new_as_creator_with_protocol_config() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let creator = member(1);
        let group = Group::new_as_creator("test-group", creator.clone(), config).unwrap();

        assert!(group.is_steward());
        assert!(group.steward_list().is_some());

        let list = group.steward_list().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&creator));
    }

    #[test]
    fn test_set_steward_list_on_joiner() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_joiner("test-group", member(1), config.clone());
        assert!(group.steward_list().is_none());

        let mems = members(&[1, 2, 3]);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        assert!(group.steward_list().is_some());
        assert_eq!(group.steward_list().unwrap().len(), 3);
    }

    /// Joiner pre-sync: no list yet → no epoch steward at any epoch.
    #[test]
    fn test_epoch_steward_no_list() {
        let group = Group::new_as_joiner("test-group", member(1), default_config());
        assert!(group.epoch_steward(0).is_none());
    }

    /// Joiner pre-sync: no list yet → not "exhausted" either.
    #[test]
    fn test_is_steward_list_exhausted_no_list() {
        let group = Group::new_as_joiner("test-group", member(1), default_config());
        assert!(!group.is_steward_list_exhausted(0));
    }

    #[test]
    fn test_generate_and_set_steward_list() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        assert_eq!(group.steward_list().unwrap().len(), 1);

        let mems = members(&[1, 2, 3, 4]);
        assert!(group.generate_and_set_steward_list(1, &mems, 4, 0).is_ok());

        let list = group.steward_list().unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list.start_epoch(), 1);
    }

    /// `is_steward()` flips to true only once the list has been generated
    /// and the node's identity sits in it.
    #[test]
    fn test_steward_flag_derived_from_list() {
        let mut group = Group::new_as_joiner("test-group", member(1), default_config());
        assert!(!group.is_steward());

        let mems = members(&[1, 2, 3]);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();
        assert!(group.is_steward());

        let outsider = Group::new_as_joiner("test-group", member(99), default_config());
        assert!(!outsider.is_steward());
    }

    #[test]
    fn test_accept_auto_approved_leave_goes_directly_to_approved() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        let leaver = member(2);
        assert_eq!(group.approved_proposals_count(), 0);
        assert_eq!(group.pending_update_count(), 0);
        assert!(!group.is_pending_self_leave(&leaver));

        assert!(group.accept_auto_approved_leave(leaver.clone()));

        // Auto-approved leaves land in `approved_proposals` directly,
        // keyed by the deterministic proposal id, and don't touch pending_updates.
        assert_eq!(group.pending_update_count(), 0);
        assert_eq!(group.approved_proposals_count(), 1);
        assert!(group.is_pending_self_leave(&leaver));
        let expected_id = auto_approved_leave_proposal_id(&leaver);
        assert!(group.approved_proposals().contains_key(&expected_id));

        // Idempotent: second accept for the same identity is a no-op.
        assert!(!group.accept_auto_approved_leave(leaver.clone()));
        assert_eq!(group.approved_proposals_count(), 1);
    }

    #[test]
    fn test_reject_all_approved_preserves_auto_approved_leaves() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        // One auto-approved leave + one consensus-approved proposal.
        let leaver = member(2);
        group.accept_auto_approved_leave(leaver.clone());
        let ban_id: ProposalId = 0xdead_beef;
        let ban = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::InviteMember(
                crate::protos::de_mls::messages::v1::InviteMember {
                    key_package_bytes: vec![0; 8],
                    identity: member(99),
                },
            )),
        };
        group.insert_approved_proposal(ban_id, ban);
        assert_eq!(group.approved_proposals_count(), 2);

        // Simulate freeze failure.
        group.reject_all_approved_proposals();

        // Auto-approved leave survives; the consensus-approved ban is gone.
        assert_eq!(group.approved_proposals_count(), 1);
        let leave_id = auto_approved_leave_proposal_id(&leaver);
        assert!(group.approved_proposals().contains_key(&leave_id));
        assert!(!group.approved_proposals().contains_key(&ban_id));
    }

    #[test]
    fn test_prune_clears_auto_approved_leave_when_member_gone() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        let leaver = member(2);
        group.accept_auto_approved_leave(leaver.clone());
        assert_eq!(group.approved_proposals_count(), 1);
        assert!(group.is_pending_self_leave(&leaver));

        // Commit merged — leaver is no longer a member.
        let after = members(&[1, 3]);
        group.prune_pending_updates_for_members(&after);

        assert_eq!(group.approved_proposals_count(), 0);
        assert!(!group.is_pending_self_leave(&leaver));
    }

    /// A pending ban on the target dedupes a subsequent self-leave —
    /// avoids two `RemoveMember` entries for the same identity in one batch.
    #[test]
    fn test_accept_auto_approved_leave_deduped_when_ban_pending() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        let target = member(2);
        let ban_remove = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(
                crate::protos::de_mls::messages::v1::RemoveMember {
                    identity: target.clone(),
                },
            )),
        };
        assert!(group.buffer_pending_update(ban_remove, 0));

        assert!(!group.accept_auto_approved_leave(target.clone()));
        assert_eq!(group.approved_proposals_count(), 0);
        assert!(!group.is_pending_self_leave(&target));
    }

    /// Live rotation skips a nominal steward who has submitted a self-leave.
    #[test]
    fn test_live_rotation_skips_pending_self_leave() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        let nominal = group.epoch_steward(0).unwrap().to_vec();
        assert_eq!(group.live_epoch_steward(0, &mems), Some(nominal.as_slice()));

        group.accept_auto_approved_leave(nominal.clone());
        let live = group.live_epoch_steward(0, &mems).unwrap();
        assert_ne!(live, nominal.as_slice());
        assert!(mems.iter().any(|m| m == live));
    }
}
