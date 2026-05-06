//! Per-group app-level state: proposal queues, steward list, freeze-round
//! candidate buffer, pending-update buffer, ECP dedup. MLS crypto state
//! lives in `OpenMlsService` alongside this.

use std::collections::{HashMap, HashSet, VecDeque};

use sha2::{Digest, Sha256};

use crate::{
    core::{
        CoreError,
        proposal_kind::ProposalKind,
        steward_list::{ProtocolConfig, StewardList},
    },
    mls_crypto::MlsService,
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
pub(crate) fn member_set(members: &[Vec<u8>]) -> HashSet<&[u8]> {
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

/// Per-group protocol state. Stewards batch commits; members vote.
/// Construct with [`Self::new_as_creator`] or [`Self::new_as_joiner`].
///
/// Owns the per-group MLS service (`Option<M>` because joiners in
/// `PendingJoin` don't have a service until the welcome arrives — at that
/// point the User attaches one via [`Self::attach_mls`]).
pub struct Group<M: MlsService> {
    /// Per-group MLS service. `None` for joiners that haven't accepted
    /// a welcome yet; otherwise `Some`.
    mls: Option<M>,
    /// The name of the group.
    group_name: String,
    /// This user's wallet identity (for deriving steward status from the list).
    self_identity: Vec<u8>,
    /// Proposals that passed consensus, waiting for steward to commit.
    approved_proposals: HashMap<ProposalId, GroupUpdateRequest>,
    /// Insertion order of `approved_proposals` (FIFO). Library proposal
    /// IDs are content-derived hashes, so sort-by-id is not temporal.
    approved_order: Vec<ProposalId>,
    /// Proposals waiting on consensus voting (created by this user).
    voting_proposals: HashMap<ProposalId, GroupUpdateRequest>,
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
    /// creation from `GroupConfig`; defaults to [`DEFAULT_MAX_REELECTION_ATTEMPTS`].
    max_reelection_attempts: u32,
    /// Bounded FIFO of proposal IDs with a locally-observed consensus outcome.
    /// Used by `apply_consensus_outcome` to drop library re-emissions and by
    /// `forward_incoming_vote` to distinguish benign late peer votes (session
    /// was trimmed after resolution) from votes for unknown proposal IDs.
    resolved_proposals: ResolvedProposalCache,
    /// When `Some(target)`, the next freeze cycle commits only the
    /// `RemoveMember(target)` entry; other approvals wait so they don't
    /// dilute the fast-removal intent.
    urgent_commit_target: Option<Vec<u8>>,
    /// While `true`, `create_commit_candidate` bypasses the `is_steward()`
    /// gate so any member can produce the recovery commit.
    recovery_mode: bool,
    /// At or below this score, a member is eligible for
    /// `SCORE_BELOW_THRESHOLD` ECP removal (RFC §Peer Scoring).
    threshold_peer_score: i64,
    /// Auto-vote default and consensus tie-break rule (RFC §Creating
    /// Voting Proposal).
    liveness_criteria_yes: bool,
    /// Max age (in epochs) of a buffered membership update before the
    /// epoch steward drops it.
    pending_update_max_epochs: u32,
}

/// Fallback ceiling on steward-election retries. One retry gives the
/// responsible proposer a second shot with a different list composition;
/// beyond that human/policy intervention is expected.
pub const DEFAULT_MAX_REELECTION_ATTEMPTS: u32 = 1;

pub const DEFAULT_THRESHOLD_PEER_SCORE: i64 = 0;

pub const DEFAULT_LIVENESS_CRITERIA_YES: bool = true;

pub const DEFAULT_PENDING_UPDATE_MAX_EPOCHS: u32 = 3;

impl<M: MlsService> Group<M> {
    fn new_base(
        group_name: &str,
        self_identity: Vec<u8>,
        protocol_config: ProtocolConfig,
        mls: Option<M>,
    ) -> Self {
        Self {
            mls,
            group_name: group_name.to_string(),
            self_identity,
            approved_proposals: HashMap::new(),
            approved_order: Vec::new(),
            voting_proposals: HashMap::new(),
            steward_list: None,
            protocol_config,
            active_emergency_ids: HashSet::new(),
            pending_removal_targets: HashSet::new(),
            committed_batch_hashes: VecDeque::new(),
            freeze_round: None,
            pending_updates: HashMap::new(),
            reelection_round: 0,
            max_reelection_attempts: DEFAULT_MAX_REELECTION_ATTEMPTS,
            resolved_proposals: ResolvedProposalCache::new(RESOLVED_PROPOSAL_CACHE_CAPACITY),
            urgent_commit_target: None,
            recovery_mode: false,
            threshold_peer_score: DEFAULT_THRESHOLD_PEER_SCORE,
            liveness_criteria_yes: DEFAULT_LIVENESS_CRITERIA_YES,
            pending_update_max_epochs: DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
        }
    }

    /// Borrow the MLS service for this group, if attached. Returns `None`
    /// for joiners in `PendingJoin` who haven't accepted a welcome yet.
    pub fn mls(&self) -> Option<&M> {
        self.mls.as_ref()
    }

    /// Attach an MLS service to this group. Used by joiners after a
    /// welcome arrives; idempotent in spirit (overwrites whatever was
    /// there — caller is responsible for not double-attaching).
    pub fn attach_mls(&mut self, mls: M) {
        self.mls = Some(mls);
    }

    /// Drop the attached MLS service and return it. Used on group leave
    /// so the caller can run service-side cleanup (`mls.delete()`).
    pub fn take_mls(&mut self) -> Option<M> {
        self.mls.take()
    }

    pub(crate) fn is_consensus_outcome_applied(&self, proposal_id: ProposalId) -> bool {
        self.resolved_proposals.contains(proposal_id)
    }

    pub(crate) fn mark_consensus_outcome_applied(&mut self, proposal_id: ProposalId) {
        self.resolved_proposals.record(proposal_id);
    }

    /// Create a new group handle for a joining member (not yet steward).
    /// The MLS service starts unattached; call [`Self::attach_mls`] after
    /// the welcome is accepted.
    pub fn new_as_joiner(
        group_name: &str,
        self_identity: Vec<u8>,
        protocol_config: ProtocolConfig,
    ) -> Self {
        Self::new_base(group_name, self_identity, protocol_config, None)
    }

    /// Create a new group handle for the group creator, initializing the
    /// steward list and attaching the supplied MLS service.
    pub fn new_as_creator(
        group_name: &str,
        creator_identity: Vec<u8>,
        protocol_config: ProtocolConfig,
        mls: M,
    ) -> Result<Self, CoreError> {
        let mut group = Self::new_base(
            group_name,
            creator_identity.clone(),
            protocol_config.clone(),
            Some(mls),
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
        let member_set = member_set(members);
        self.steward_list.as_ref().and_then(|l| {
            l.live_epoch_steward(epoch, |candidate| {
                self.is_steward_eligible_in(candidate, &member_set)
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
            Some(l) => {
                let member_set = member_set(members);
                l.live_epoch_and_backup(epoch, |c| self.is_steward_eligible_in(c, &member_set))
            }
            None => (None, None),
        }
    }

    fn is_steward_eligible_in(&self, candidate: &[u8], member_set: &HashSet<&[u8]>) -> bool {
        !self.is_pending_removal(candidate) && member_set.contains(candidate)
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

    // ─────────────────────────── Steward List Operations ───────────────────────────

    pub fn steward_list(&self) -> Option<&StewardList> {
        self.steward_list.as_ref()
    }

    pub fn protocol_config(&self) -> &ProtocolConfig {
        &self.protocol_config
    }

    pub fn set_protocol_config(&mut self, config: ProtocolConfig) {
        self.protocol_config = config;
    }

    pub fn epoch_steward(&self, epoch: u64) -> Option<&[u8]> {
        self.steward_list
            .as_ref()
            .and_then(|l| l.epoch_steward(epoch))
    }

    /// Steward identities filtered by the steward-eligibility predicate
    /// (MLS-present and not queued for removal). Joiners receive this view
    /// via `GroupSync` so they don't inherit ghosts or members whose
    /// removal is queued. Order is preserved from the canonical list.
    /// Returns an empty `Vec` if no list is set.
    pub(crate) fn live_steward_members(&self, members: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let Some(list) = self.steward_list.as_ref() else {
            return Vec::new();
        };
        let member_set = member_set(members);
        list.members()
            .iter()
            .filter(|m| self.is_steward_eligible_in(m, &member_set))
            .cloned()
            .collect()
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

    // ─────────────────────────── Reelection / GroupSync-Tunable Fields ───────────────────────────

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

    pub fn max_reelection_attempts(&self) -> u32 {
        self.max_reelection_attempts
    }

    pub fn set_max_reelection_attempts(&mut self, max: u32) {
        self.max_reelection_attempts = max;
    }

    pub fn threshold_peer_score(&self) -> i64 {
        self.threshold_peer_score
    }

    pub fn set_threshold_peer_score(&mut self, threshold: i64) {
        self.threshold_peer_score = threshold;
    }

    pub fn liveness_criteria_yes(&self) -> bool {
        self.liveness_criteria_yes
    }

    pub fn set_liveness_criteria_yes(&mut self, value: bool) {
        self.liveness_criteria_yes = value;
    }

    pub fn pending_update_max_epochs(&self) -> u32 {
        self.pending_update_max_epochs
    }

    pub fn set_pending_update_max_epochs(&mut self, value: u32) {
        self.pending_update_max_epochs = value;
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
pub(crate) mod test_stubs {
    use openmls::prelude::CredentialWithKey;
    use openmls_basic_credential::SignatureKeyPair;

    use crate::mls_crypto::{
        CommitCandidate as MlsCommitCandidate, DecryptResult, GroupUpdate, IdentityProvider,
        MlsError, MlsMessageKind, MlsService, StagedCandidateResult,
    };

    /// Test-only no-op identity; only `identity_bytes` and `identity_display`
    /// are ever called from `Group`'s pure-state tests, so the credential
    /// and signer accessors are stubbed with `unreachable!()`.
    pub(crate) struct NoopIdentity;

    impl IdentityProvider for NoopIdentity {
        fn identity_bytes(&self) -> &[u8] {
            &[]
        }
        fn identity_display(&self) -> &str {
            "noop"
        }
        fn credential(&self) -> &CredentialWithKey {
            unreachable!("NoopIdentity::credential is not used in pure-state Group tests")
        }
        fn signer(&self) -> &SignatureKeyPair {
            unreachable!("NoopIdentity::signer is not used in pure-state Group tests")
        }
    }

    /// Test-only no-op MLS service. `Group`'s pure-state tests never call
    /// MLS operations, so every protocol method is a stub. If a test
    /// reaches one of these, it's the wrong test and the panic message
    /// will say so.
    pub(crate) struct NoopMls {
        identity: NoopIdentity,
        group_id: String,
    }

    impl NoopMls {
        pub(crate) fn new(group_id: &str) -> Self {
            Self {
                identity: NoopIdentity,
                group_id: group_id.to_string(),
            }
        }
    }

    impl MlsService for NoopMls {
        type Identity = NoopIdentity;
        fn identity(&self) -> &Self::Identity {
            &self.identity
        }
        fn group_id(&self) -> &str {
            &self.group_id
        }
        fn delete(&self) -> Result<(), MlsError> {
            Ok(())
        }
        fn members(&self) -> Result<Vec<Vec<u8>>, MlsError> {
            Ok(Vec::new())
        }
        fn is_member(&self, _identity: &[u8]) -> bool {
            false
        }
        fn current_epoch(&self) -> Result<u64, MlsError> {
            Ok(0)
        }
        fn create_commit_candidate(
            &self,
            _updates: &[GroupUpdate],
        ) -> Result<MlsCommitCandidate, MlsError> {
            unreachable!("NoopMls::create_commit_candidate not used in pure-state tests")
        }
        fn merge_own_commit(&self) -> Result<(), MlsError> {
            unreachable!()
        }
        fn discard_own_commit(&self) -> Result<(), MlsError> {
            unreachable!()
        }
        fn apply_remote_candidate(
            &self,
            _proposals: &[Vec<u8>],
            _commit_bytes: &[u8],
        ) -> Result<StagedCandidateResult, MlsError> {
            unreachable!()
        }
        fn merge_staged_commit(&self) -> Result<(), MlsError> {
            unreachable!()
        }
        fn discard_staged_commit(&self) -> Result<(), MlsError> {
            unreachable!()
        }
        fn encrypt(&self, _plaintext: &[u8]) -> Result<Vec<u8>, MlsError> {
            unreachable!()
        }
        fn decrypt_application_only(&self, _ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
            unreachable!()
        }
        fn decrypt(&self, _ciphertext: &[u8]) -> Result<DecryptResult, MlsError> {
            unreachable!()
        }
        fn inspect_message_kind(&self, _message_bytes: &[u8]) -> Result<MlsMessageKind, MlsError> {
            unreachable!()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::group::test_stubs::NoopMls;
    use crate::protos::de_mls::messages::v1::{InviteMember, RemoveMember};

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    fn default_config() -> ProtocolConfig {
        ProtocolConfig::new(1, 5).unwrap()
    }

    /// Test convenience: build a creator-side `Group<NoopMls>`.
    fn creator_group(name: &str, identity: Vec<u8>, config: ProtocolConfig) -> Group<NoopMls> {
        Group::new_as_creator(name, identity, config, NoopMls::new(name)).unwrap()
    }

    /// Test convenience: build a joiner-side `Group<NoopMls>` (no MLS service yet).
    fn joiner_group(name: &str, identity: Vec<u8>, config: ProtocolConfig) -> Group<NoopMls> {
        Group::new_as_joiner(name, identity, config)
    }

    #[test]
    fn test_new_as_creator_with_protocol_config() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let creator = member(1);
        let group = creator_group("test-group", creator.clone(), config);

        assert!(group.is_steward());
        assert!(group.steward_list().is_some());

        let list = group.steward_list().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&creator));
    }

    #[test]
    fn test_set_steward_list_on_joiner() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = joiner_group("test-group", member(1), config.clone());
        assert!(group.steward_list().is_none());

        let mems = members(&[1, 2, 3]);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        assert!(group.steward_list().is_some());
        assert_eq!(group.steward_list().unwrap().len(), 3);
    }

    /// Joiner pre-sync: no list yet → no epoch steward at any epoch.
    #[test]
    fn test_epoch_steward_no_list() {
        let group = joiner_group("test-group", member(1), default_config());
        assert!(group.epoch_steward(0).is_none());
    }

    /// Joiner pre-sync: no list yet → not "exhausted" either.
    #[test]
    fn test_is_steward_list_exhausted_no_list() {
        let group = joiner_group("test-group", member(1), default_config());
        assert!(!group.is_steward_list_exhausted(0));
    }

    #[test]
    fn test_generate_and_set_steward_list() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = creator_group("test-group", member(1), config);
        assert_eq!(group.steward_list().unwrap().len(), 1);

        let mems = members(&[1, 2, 3, 4]);
        assert!(group.generate_and_set_steward_list(1, &mems, 4, 0).is_ok());

        let list = group.steward_list().unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list.election_epoch(), 1);
    }

    /// `is_steward()` flips to true only once the list has been generated
    /// and the node's identity sits in it.
    #[test]
    fn test_steward_flag_derived_from_list() {
        let mut group = joiner_group("test-group", member(1), default_config());
        assert!(!group.is_steward());

        let mems = members(&[1, 2, 3]);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();
        assert!(group.is_steward());

        let outsider = joiner_group("test-group", member(99), default_config());
        assert!(!outsider.is_steward());
    }

    fn insert_self_leave(group: &mut Group<NoopMls>, identity: &[u8]) {
        let remove = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: identity.to_vec(),
            })),
        };
        group.insert_approved_proposal(auto_approved_leave_proposal_id(identity), remove);
    }

    #[test]
    fn test_reject_all_approved_preserves_self_leave_entry() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = creator_group("test-group", member(1), config);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

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
        let config = ProtocolConfig::new(1, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = creator_group("test-group", member(1), config);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

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
        let mut group = creator_group("g", member(1), default_config());
        assert!(!group.is_consensus_outcome_applied(42));
        group.mark_consensus_outcome_applied(42);
        assert!(group.is_consensus_outcome_applied(42));
    }

    /// Live rotation skips a nominal steward who has submitted a self-leave.
    #[test]
    fn test_live_rotation_skips_pending_self_leave() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = creator_group("test-group", member(1), config);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        let nominal = group.epoch_steward(0).unwrap().to_vec();
        assert_eq!(group.live_epoch_steward(0, &mems), Some(nominal.as_slice()));

        insert_self_leave(&mut group, &nominal);
        let live = group.live_epoch_steward(0, &mems).unwrap();
        assert_ne!(live, nominal.as_slice());
        assert!(mems.iter().any(|m| m == live));
    }

    fn insert_remove_member(group: &mut Group<NoopMls>, target: &[u8], proposal_id: ProposalId) {
        let remove = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: target.to_vec(),
            })),
        };
        group.insert_approved_proposal(proposal_id, remove);
    }

    /// Live rotation skips a nominal steward whose removal is queued under a
    /// non-self-leave proposal id (ban / ECP-derived).
    #[test]
    fn test_live_rotation_skips_ban_pending_removal() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = creator_group("test-group", member(1), config);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

        let nominal = group.epoch_steward(0).unwrap().to_vec();
        assert_eq!(group.live_epoch_steward(0, &mems), Some(nominal.as_slice()));

        // Removal under an arbitrary proposal id (not the deterministic
        // self-leave id) — the narrower self-leave predicate would miss it.
        insert_remove_member(&mut group, &nominal, 0xdead_beef);
        assert!(!group.is_pending_self_leave(&nominal));
        assert!(group.is_pending_removal(&nominal));

        let live = group.live_epoch_steward(0, &mems).unwrap();
        assert_ne!(live, nominal.as_slice());
        assert!(mems.iter().any(|m| m == live));
    }

    /// `live_epoch_and_backup` skips a draining nominal *and* a draining
    /// backup, returning the next two eligible distinct stewards.
    #[test]
    fn test_live_epoch_and_backup_skips_draining_pair() {
        let config = ProtocolConfig::new(4, 4).unwrap();
        let mems = members(&[1, 2, 3, 4]);
        let mut group = creator_group("test-group", member(1), config);
        group.generate_and_set_steward_list(0, &mems, 4, 0).unwrap();

        let nominal = group.epoch_steward(0).unwrap().to_vec();
        let backup = group
            .steward_list()
            .unwrap()
            .backup_steward(0)
            .unwrap()
            .to_vec();

        insert_remove_member(&mut group, &nominal, 1);
        insert_remove_member(&mut group, &backup, 2);

        let (live_epoch, live_backup) = group.live_epoch_and_backup(0, &mems);
        let live_epoch = live_epoch.expect("eligible epoch steward").to_vec();
        let live_backup = live_backup.expect("eligible backup steward").to_vec();
        assert_ne!(live_epoch, nominal);
        assert_ne!(live_epoch, backup);
        assert_ne!(live_backup, nominal);
        assert_ne!(live_backup, backup);
        assert_ne!(live_epoch, live_backup);
    }

    /// `RemoveMember` proposals survive a freeze failure regardless of
    /// source; Add proposals are dropped.
    #[test]
    fn test_reject_all_approved_preserves_all_remove_member() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let mems = members(&[1, 2, 3, 4]);
        let mut group = creator_group("test-group", member(1), config);
        group.generate_and_set_steward_list(0, &mems, 3, 0).unwrap();

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
        let mut group = creator_group("g", member(1), default_config());

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
        let mut group = creator_group("g", member(1), default_config());
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
        let mut group = creator_group("g", member(1), default_config());
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

    fn buffer_remove_at(group: &mut Group<NoopMls>, target: &[u8], epoch: u64) {
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
        let mut group = creator_group("g", member(1), default_config());
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
        let mut group = creator_group("g", member(1), default_config());
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
