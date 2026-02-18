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

use std::collections::{HashMap, VecDeque};

use crate::core::group_update_handle::{CurrentEpochProposals, ProposalId};
use crate::protos::de_mls::messages::v1::GroupUpdateRequest;

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
}

impl GroupHandle {
    /// Create a new group handle for an existing group (joining).
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    pub fn new_for_join(group_name: &str) -> Self {
        Self {
            group_name: group_name.to_string(),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: false,
            mls_initialized: false,
            proposals: CurrentEpochProposals::new(),
            current_epoch: 0,
            steward_identity: None,
        }
    }

    /// Create a new group handle for creating a new group (as steward).
    ///
    /// The MLS group should be created via `mls_service.create_group()` first.
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `creator_identity` - The wallet bytes of the creator (who is the initial steward)
    pub fn new_as_creator(group_name: &str, creator_identity: Vec<u8>) -> Self {
        Self {
            group_name: group_name.to_string(),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: true,
            mls_initialized: true,
            proposals: CurrentEpochProposals::new(),
            current_epoch: 0,
            steward_identity: Some(creator_identity),
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
    ///
    /// Owners are responsible for broadcasting the proposal to peers and
    /// must include the full `Proposal` when casting their vote.
    pub fn is_owner_of_proposal(&self, proposal_id: ProposalId) -> bool {
        self.proposals.is_owner_of_proposal(proposal_id)
    }

    /// Get the count of approved proposals waiting to be committed.
    ///
    /// The steward checks this to determine when to create a batch commit.
    pub fn approved_proposals_count(&self) -> usize {
        self.proposals.approved_proposals_count()
    }

    /// Get a copy of all approved proposals.
    ///
    /// Called by the steward when creating a batch commit. The proposals
    /// are sorted by SHA256 hash for deterministic ordering.
    pub fn approved_proposals(&self) -> HashMap<ProposalId, GroupUpdateRequest> {
        self.proposals.approved_proposals()
    }

    /// Move a proposal from voting to approved queue.
    ///
    /// Called when consensus is reached with `result = true` and this user
    /// is the proposal owner.
    pub fn mark_proposal_as_approved(&mut self, proposal_id: ProposalId) {
        self.proposals.move_proposal_to_approved(proposal_id);
    }

    /// Remove a proposal from the voting queue (rejected or failed).
    ///
    /// Called when consensus is reached with `result = false` or when
    /// consensus fails (timeout, insufficient votes).
    pub fn mark_proposal_as_rejected(&mut self, proposal_id: ProposalId) {
        self.proposals.remove_voting_proposal(proposal_id);
    }

    /// Store a newly created proposal in the voting queue.
    ///
    /// Called after `start_voting()` successfully creates a proposal.
    /// The proposal remains here until consensus completes.
    pub fn store_voting_proposal(&mut self, proposal_id: ProposalId, proposal: GroupUpdateRequest) {
        self.proposals.add_voting_proposal(proposal_id, proposal);
    }

    /// Insert a proposal directly into the approved queue.
    ///
    /// Called by non-owners when they receive a consensus result for a
    /// proposal they didn't create. They fetch the payload from the
    /// consensus service and insert it directly as approved.
    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: GroupUpdateRequest,
    ) {
        self.proposals.add_proposal(proposal_id, proposal);
    }

    /// Remove a single proposal from the approved queue.
    ///
    /// Used for proposals that don't produce MLS operations (e.g., emergency criteria
    /// proposals are consensus-only and should not be included in MLS commits).
    pub fn remove_approved_proposal(&mut self, proposal_id: ProposalId) {
        self.proposals.remove_approved_proposal(proposal_id);
    }

    /// Clear approved proposals after a commit, archiving to history.
    ///
    /// Called after a batch commit is successfully applied. The proposals
    /// are moved to `epoch_history` for UI display (up to 10 epochs retained).
    /// Also advances the epoch counter.
    pub fn clear_approved_proposals(&mut self) {
        self.proposals.clear_approved_proposals();
        self.current_epoch += 1;
    }

    /// Get the epoch history (past batches of approved proposals).
    ///
    /// Returns up to 10 past epochs, most recent last. Useful for UI
    /// to show recent membership changes.
    pub fn epoch_history(&self) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
        self.proposals.epoch_history()
    }
}
