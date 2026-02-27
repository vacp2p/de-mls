//! Per-group state container for app-level operations.
//!
//! This module provides [`GroupHandle`], which holds app-level state for
//! a single group: proposal tracking, steward status, and deduplication ID.
//!
//! **Note**: MLS cryptographic state is managed by `MlsService` internally.
//! This handle only tracks application-layer concerns.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                        GroupHandle                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  group_name      │  Human-readable group identifier         │
//! │  app_id          │  UUID for message deduplication          │
//! │  steward         │  Whether this user batches commits       │
//! │  proposals       │  Voting + approved proposal queues       │
//! │  mls_initialized │  Whether MLS state exists in MlsService  │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Lifecycle
//!
//! **Creating a group (as steward):**
//! ```ignore
//! mls_service.create_group(group_name)?;
//! let handle = GroupHandle::new_as_creator(group_name);
//! // handle.is_steward() == true
//! ```
//!
//! **Joining a group (as member):**
//! ```ignore
//! let handle = GroupHandle::new_for_join(group_name);
//! // handle.is_steward() == false
//!
//! // Later, when welcome is received:
//! mls_service.join_group(&welcome)?;
//! handle.set_mls_initialized();
//! ```
//!
//! # Proposal Flow
//!
//! The handle tracks proposals through their lifecycle:
//!
//! ```text
//! 1. store_voting_proposal()   →  Proposal created, waiting for votes
//! 2. mark_proposal_as_approved()  →  Consensus reached, ready for commit
//!    OR mark_proposal_as_rejected()  →  Consensus rejected, discard
//! 3. approved_proposals()      →  Steward reads approved proposals
//! 4. clear_approved_proposals()  →  After commit, archive to history
//! ```
//!
//! Non-owners (members who didn't create the proposal) use:
//! - `insert_approved_proposal()` - Add proposal directly to approved queue

use std::collections::{BTreeSet, HashMap, VecDeque};

use crate::core::group_update_handle::{CurrentEpochProposals, ProposalId};
use crate::protos::de_mls::messages::v1::{CommitCandidate, GroupUpdateRequest};

/// Maximum number of committed batch hashes to remember for dedup.
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
    #[allow(dead_code)] // retained for future subset-check use
    pub eligible_proposal_ids: BTreeSet<ProposalId>,
    pub selection_locked: bool,
    pub candidates: Vec<BufferedCommitCandidate>,
}

/// Handle for a single MLS group's app-level state.
///
/// Contains state needed for group operations:
/// - Application ID for message deduplication across instances
/// - Steward flag indicating whether this user batches commits
/// - Proposal queues for tracking voting and approved proposals
///
/// **Note**: MLS cryptographic state (encryption keys, group members) is
/// managed by `MlsService`. Use `mls_service.encrypt()`, `mls_service.decrypt()`,
/// etc. for MLS operations.
///
/// # Thread Safety
///
/// The `GroupHandle` should be wrapped in `RwLock` or similar by the
/// application layer (see `User.groups` in the app module).
///
/// # Steward vs Member
///
/// - **Steward**: Creates proposals, collects votes, batches approved proposals
///   into MLS commits. Created via `new_as_creator()`.
/// - **Member**: Votes on proposals, receives commits. Created via `new_for_join()`.
#[derive(Clone, Debug)]
pub struct GroupHandle {
    /// The name of the group.
    group_name: String,
    /// Unique application instance ID for message deduplication.
    app_id: Vec<u8>,
    /// Whether this user is the steward for this group.
    steward: bool,
    /// Whether MLS state is initialized in MlsService.
    mls_initialized: bool,
    /// Proposal for current steward epoch.
    proposals: CurrentEpochProposals,
    /// Current epoch counter (incremented on each commit).
    current_epoch: u64,
    /// Identity (wallet bytes) of the current steward.
    steward_identity: Option<Vec<u8>>,
    /// Recent commit hashes for dedup.
    committed_batch_hashes: VecDeque<Vec<u8>>,
    /// Freeze-round candidate buffer for deterministic selection.
    freeze_round: Option<FreezeRound>,
    /// Whether subset candidates are allowed during selection.
    allow_subset_candidates: bool,
}

impl GroupHandle {
    fn new_base(group_name: &str) -> Self {
        Self {
            group_name: group_name.to_string(),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: false,
            mls_initialized: false,
            proposals: CurrentEpochProposals::new(),
            current_epoch: 0,
            steward_identity: None,
            committed_batch_hashes: VecDeque::new(),
            freeze_round: None,
            allow_subset_candidates: false,
        }
    }

    /// Create a new group handle for an existing group (joining).
    pub fn new_for_join(group_name: &str) -> Self {
        Self::new_base(group_name)
    }

    /// Create a new group handle for creating a new group (as steward).
    ///
    /// The MLS group should be created via `mls_service.create_group()` first.
    pub fn new_as_creator(group_name: &str, creator_identity: Vec<u8>) -> Self {
        let mut handle = Self::new_base(group_name);
        handle.steward = true;
        handle.mls_initialized = true;
        handle.steward_identity = Some(creator_identity);
        handle
    }

    /// Get the group name.
    pub fn group_name(&self) -> &str {
        &self.group_name
    }

    /// Get the group name as bytes.
    pub fn group_name_bytes(&self) -> &[u8] {
        self.group_name.as_bytes()
    }

    /// Get the application ID.
    pub fn app_id(&self) -> &[u8] {
        &self.app_id
    }

    /// Check if this user is the steward.
    pub fn is_steward(&self) -> bool {
        self.steward
    }

    /// Check if the MLS group is initialized.
    pub fn is_mls_initialized(&self) -> bool {
        self.mls_initialized
    }

    /// Mark MLS as initialized (called after joining via welcome).
    pub fn set_mls_initialized(&mut self) {
        self.mls_initialized = true;
    }

    /// Get the steward's identity bytes, if known.
    pub fn steward_identity(&self) -> Option<&[u8]> {
        self.steward_identity.as_deref()
    }

    /// Set the steward's identity bytes.
    pub fn set_steward_identity(&mut self, identity: Vec<u8>) {
        self.steward_identity = Some(identity);
    }

    /// Get the current epoch number.
    pub fn current_epoch(&self) -> u64 {
        self.current_epoch
    }

    /// Advance to the next epoch (called when a commit is processed).
    pub fn advance_epoch(&mut self) {
        self.current_epoch += 1;
    }

    /// Become the steward of this group.
    pub fn become_steward(&mut self) {
        self.steward = true;
    }

    /// Resign as steward of this group.
    pub fn resign_steward(&mut self) {
        self.steward = false;
    }

    /// Set whether subset candidates are allowed in freeze selection.
    pub fn set_allow_subset_candidates(&mut self, allow_subset: bool) {
        self.allow_subset_candidates = allow_subset;
    }

    /// Whether subset candidates are allowed in freeze selection.
    pub fn allow_subset_candidates(&self) -> bool {
        self.allow_subset_candidates
    }

    // ─────────────────────────── Proposal Handle Operations ───────────────────────────

    /// Check if this user owns (created) the given proposal.
    pub fn is_owner_of_proposal(&self, proposal_id: ProposalId) -> bool {
        self.proposals.is_owner_of_proposal(proposal_id)
    }

    /// Get the count of approved proposals waiting to be committed.
    pub fn approved_proposals_count(&self) -> usize {
        self.proposals.approved_proposals_count()
    }

    /// Get a copy of all approved proposals.
    pub fn approved_proposals(&self) -> HashMap<ProposalId, GroupUpdateRequest> {
        self.proposals.approved_proposals()
    }

    /// Check if the proposal exists in the approved queue.
    pub(crate) fn has_approved_proposal(&self, proposal_id: ProposalId) -> bool {
        self.proposals.has_approved_proposal(proposal_id)
    }

    /// Move a proposal from voting to approved queue.
    pub fn mark_proposal_as_approved(&mut self, proposal_id: ProposalId) {
        self.proposals.move_proposal_to_approved(proposal_id);
    }

    /// Remove a proposal from the voting queue (rejected or failed).
    pub fn mark_proposal_as_rejected(&mut self, proposal_id: ProposalId) {
        self.proposals.remove_voting_proposal(proposal_id);
    }

    /// Store a newly created proposal in the voting queue.
    pub fn store_voting_proposal(&mut self, proposal_id: ProposalId, proposal: GroupUpdateRequest) {
        self.proposals.add_voting_proposal(proposal_id, proposal);
    }

    /// Insert a proposal directly into the approved queue.
    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: GroupUpdateRequest,
    ) {
        self.proposals.add_proposal(proposal_id, proposal);
    }

    /// Remove a single proposal from the approved queue.
    pub fn remove_approved_proposal(&mut self, proposal_id: ProposalId) {
        self.proposals.remove_approved_proposal(proposal_id);
    }

    /// Clear approved proposals after a commit, archiving to history.
    /// Also advances the epoch counter.
    pub fn clear_approved_proposals(&mut self) {
        self.proposals.clear_approved_proposals();
        self.current_epoch += 1;
    }

    /// Reject all currently approved proposals without advancing epoch.
    ///
    /// Used when freeze times out with no valid candidate selected.
    pub fn reject_all_approved_proposals(&mut self) {
        let ids: Vec<ProposalId> = self
            .proposals
            .approved_proposals()
            .keys()
            .copied()
            .collect();
        for id in ids {
            self.proposals.remove_approved_proposal(id);
        }
    }

    /// Reject all currently voting proposals without advancing epoch.
    ///
    /// Used alongside `reject_all_approved_proposals()` when freeze times out
    /// with no valid candidate selected.
    pub fn reject_all_voting_proposals(&mut self) {
        self.proposals.clear_voting_proposals();
    }

    /// Get the epoch history (past batches of approved proposals).
    pub fn epoch_history(&self) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
        self.proposals.epoch_history()
    }

    // ─────────────────────────── Freeze Round Operations ───────────────────────────

    fn build_freeze_round(&self, epoch: u64) -> FreezeRound {
        let eligible_proposal_ids = self
            .proposals
            .approved_proposals()
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();

        FreezeRound {
            epoch,
            eligible_proposal_ids,
            selection_locked: false,
            candidates: Vec::new(),
        }
    }

    /// Ensure a freeze round exists for the current epoch.
    ///
    /// If absent or stale, initializes one with the current approved proposal IDs.
    pub(crate) fn ensure_freeze_round(&mut self) {
        let epoch = self.current_epoch;
        if matches!(self.freeze_round, Some(ref round) if round.epoch == epoch) {
            return;
        }
        self.freeze_round = Some(self.build_freeze_round(epoch));
    }

    /// Start a new freeze round for the current epoch.
    ///
    /// Existing round state is replaced.
    pub fn start_freeze_round(&mut self) {
        self.freeze_round = Some(self.build_freeze_round(self.current_epoch));
    }

    /// Add a validated candidate to the active freeze round.
    ///
    /// Returns `true` if buffered, `false` if ignored (locked round or duplicate).
    pub(crate) fn buffer_freeze_candidate(&mut self, candidate: BufferedCommitCandidate) -> bool {
        self.ensure_freeze_round();
        let Some(round) = self.freeze_round.as_mut() else {
            return false;
        };

        if round.epoch != self.current_epoch || round.selection_locked {
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
    pub(crate) fn lock_freeze_round_selection(&mut self) {
        if let Some(round) = self.freeze_round.as_mut() {
            if round.epoch == self.current_epoch {
                round.selection_locked = true;
            }
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

    // ─────────────────────────── Dedup Operations ───────────────────────────

    /// Check if a commit hash has already been committed (in committed history).
    ///
    /// Note: freeze round buffer dedup is handled separately by `buffer_freeze_candidate`.
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
}
