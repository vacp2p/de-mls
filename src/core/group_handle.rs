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
use crate::protos::de_mls::messages::v1::{BatchProposalsMessage, GroupUpdateRequest};

/// Maximum number of committed batch hashes to remember for dedup.
const MAX_COMMITTED_HASHES: usize = 10;

/// Policy governing quarantine behavior for unsolicited batches.
///
/// Lives in core because it governs core protocol behavior (quarantine eviction,
/// epoch expiry). Integrators can configure it at group creation time.
#[derive(Clone, Debug)]
pub struct QuarantinePolicy {
    /// Maximum number of quarantined batches per group.
    pub max_items: usize,
    /// Maximum epoch age before quarantined entries expire.
    pub max_epoch_age: u64,
}

impl Default for QuarantinePolicy {
    fn default() -> Self {
        Self {
            max_items: 5,
            max_epoch_age: 2,
        }
    }
}

/// A batch that arrived before local proposals were ready.
///
/// Stored temporarily until consensus delivers matching proposals,
/// at which point it can be re-processed.
#[derive(Clone, Debug)]
pub(crate) struct QuarantinedBatch {
    pub batch_msg: BatchProposalsMessage,
    pub commit_hash: Vec<u8>,
    pub batch_fingerprint: Vec<u8>,
    pub quarantined_at_epoch: u64,
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
    /// Batches received before local proposals were ready (bounded buffer).
    quarantine: VecDeque<QuarantinedBatch>,
    /// Recent (commit_hash, batch_fingerprint) pairs for dedup.
    committed_batch_hashes: VecDeque<(Vec<u8>, Vec<u8>)>,
    /// Quarantine policy for this group.
    quarantine_policy: QuarantinePolicy,
}

impl GroupHandle {
    /// Create a new group handle for an existing group (joining) with default policy.
    pub fn new_for_join(group_name: &str) -> Self {
        Self::new_for_join_with_policy(group_name, QuarantinePolicy::default())
    }

    /// Create a new group handle for an existing group (joining) with custom policy.
    pub fn new_for_join_with_policy(group_name: &str, policy: QuarantinePolicy) -> Self {
        Self {
            group_name: group_name.to_string(),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: false,
            mls_initialized: false,
            proposals: CurrentEpochProposals::new(),
            current_epoch: 0,
            steward_identity: None,
            quarantine: VecDeque::new(),
            committed_batch_hashes: VecDeque::new(),
            quarantine_policy: policy,
        }
    }

    /// Create a new group handle for creating a new group (as steward) with default policy.
    ///
    /// The MLS group should be created via `mls_service.create_group()` first.
    pub fn new_as_creator(group_name: &str, creator_identity: Vec<u8>) -> Self {
        Self::new_as_creator_with_policy(group_name, creator_identity, QuarantinePolicy::default())
    }

    /// Create a new group handle for creating a new group (as steward) with custom policy.
    pub fn new_as_creator_with_policy(
        group_name: &str,
        creator_identity: Vec<u8>,
        policy: QuarantinePolicy,
    ) -> Self {
        Self {
            group_name: group_name.to_string(),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: true,
            mls_initialized: true,
            proposals: CurrentEpochProposals::new(),
            current_epoch: 0,
            steward_identity: Some(creator_identity),
            quarantine: VecDeque::new(),
            committed_batch_hashes: VecDeque::new(),
            quarantine_policy: policy,
        }
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

    /// Get the epoch history (past batches of approved proposals).
    pub fn epoch_history(&self) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
        self.proposals.epoch_history()
    }

    // ─────────────────────────── Quarantine Operations ───────────────────────────

    /// Add a batch to quarantine. Evicts oldest entry if at capacity.
    pub(crate) fn quarantine_batch(&mut self, batch: QuarantinedBatch) {
        if self.quarantine.len() >= self.quarantine_policy.max_items {
            self.quarantine.pop_front();
        }
        self.quarantine.push_back(batch);
    }

    /// Remove and return the first quarantined batch whose proposal IDs match current
    /// approved proposals.
    pub(crate) fn take_matching_quarantine(&mut self) -> Option<QuarantinedBatch> {
        // Enforce age policy before trying to match entries.
        self.expire_quarantine();

        let local_ids: BTreeSet<ProposalId> = self
            .proposals
            .approved_proposals()
            .keys()
            .copied()
            .collect();
        if local_ids.is_empty() {
            return None;
        }

        let pos = self.quarantine.iter().position(|entry| {
            let batch_ids: BTreeSet<ProposalId> =
                entry.batch_msg.proposal_ids.iter().copied().collect();
            batch_ids == local_ids
        });

        pos.and_then(|i| self.quarantine.remove(i))
    }

    /// Drop quarantine entries older than `max_epoch_age` epochs.
    pub(crate) fn expire_quarantine(&mut self) {
        let current = self.current_epoch;
        let max_age = self.quarantine_policy.max_epoch_age;
        self.quarantine
            .retain(|entry| current.saturating_sub(entry.quarantined_at_epoch) < max_age);
    }

    /// Check if a batch with the given hashes is a duplicate (in quarantine or committed history).
    pub(crate) fn is_duplicate_batch(&self, commit_hash: &[u8], fingerprint: &[u8]) -> bool {
        // Check committed history
        if self
            .committed_batch_hashes
            .iter()
            .any(|(ch, fp)| ch == commit_hash && fp == fingerprint)
        {
            return true;
        }
        // Check quarantine
        self.quarantine
            .iter()
            .any(|entry| entry.commit_hash == commit_hash && entry.batch_fingerprint == fingerprint)
    }

    /// Record a committed batch's hashes for future dedup.
    pub(crate) fn record_committed_batch(&mut self, commit_hash: Vec<u8>, fingerprint: Vec<u8>) {
        if self.committed_batch_hashes.len() >= MAX_COMMITTED_HASHES {
            self.committed_batch_hashes.pop_front();
        }
        self.committed_batch_hashes
            .push_back((commit_hash, fingerprint));
    }

    /// Number of quarantined batches.
    pub fn quarantine_len(&self) -> usize {
        self.quarantine.len()
    }

    /// Whether any batches are quarantined.
    pub fn has_quarantined(&self) -> bool {
        !self.quarantine.is_empty()
    }
}
