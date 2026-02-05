//! GroupHandle - per-group state container.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::core::group_update_handle::{CurrentEpochProposals, ProposalId};
use crate::protos::de_mls::messages::v1::GroupUpdateRequest;
use mls_crypto::MlsGroupHandle;

/// Handle for a single MLS group.
///
/// The `GroupHandle` contains all state needed for group operations:
/// - MLS group handle for cryptographic operations
/// - Application ID for message routing
/// - Optional steward for proposal management
/// - Key package sharing status
///
/// # Thread Safety
///
/// The `GroupHandle` is designed to be used with external synchronization.
/// The MLS group handle is wrapped in an `Arc<Mutex>` for safe concurrent access.
#[derive(Clone, Debug)]
pub struct GroupHandle {
    /// The name of the group.
    group_name: String,
    /// The MLS group handle (None until group is joined/created).
    mls_handle: Option<Arc<Mutex<MlsGroupHandle>>>,
    /// Unique application instance ID for message deduplication.
    app_id: Vec<u8>,
    /// Optional steward for this group (per-group, can change dynamically).
    steward: bool,
    /// Proposal for current steward epoch
    proposals: CurrentEpochProposals,
}

impl GroupHandle {
    /// Create a new group handle for an existing group (joining).
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    pub fn new_for_join(group_name: &str) -> Self {
        Self {
            group_name: group_name.to_string(),
            mls_handle: None,
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: false,
            proposals: CurrentEpochProposals::new(),
        }
    }

    /// Create a new group handle for creating a new group (as steward).
    ///
    /// # Arguments
    /// * `group_name` - The name of the group
    /// * `mls_handle` - The MLS group handle
    pub fn new_as_creator(group_name: &str, mls_handle: MlsGroupHandle) -> Self {
        Self {
            group_name: group_name.to_string(),
            mls_handle: Some(Arc::new(Mutex::new(mls_handle))),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            steward: true,
            proposals: CurrentEpochProposals::new(),
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

    /// Check if this group has a steward.
    pub fn is_steward(&self) -> bool {
        self.steward
    }

    /// Check if the MLS group is initialized.
    pub fn is_mls_initialized(&self) -> bool {
        self.mls_handle.is_some()
    }

    /// Get a reference to the MLS handle.
    pub fn mls_handle(&self) -> Option<&Arc<Mutex<MlsGroupHandle>>> {
        self.mls_handle.as_ref()
    }

    /// Set the MLS group handle (used when joining a group).
    pub fn set_mls_handle(&mut self, handle: MlsGroupHandle) {
        self.mls_handle = Some(Arc::new(Mutex::new(handle)));
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

    pub fn is_owner_of_proposal(&self, proposal_id: ProposalId) -> bool {
        self.proposals.is_owner_of_proposal(proposal_id)
    }

    /// Get the count of approved proposals.
    pub fn approved_proposals_count(&self) -> usize {
        self.proposals.approved_proposals_count()
    }

    /// Get the approved proposals.
    pub fn approved_proposals(&self) -> HashMap<ProposalId, GroupUpdateRequest> {
        self.proposals.approved_proposals()
    }

    pub fn mark_proposal_as_approved(&mut self, proposal_id: ProposalId) {
        self.proposals.move_proposal_to_approved(proposal_id);
    }

    pub fn mark_proposal_as_rejected(&mut self, proposal_id: ProposalId) {
        self.proposals.remove_voting_proposal(proposal_id);
    }

    pub fn store_voting_proposal(&mut self, proposal_id: ProposalId, proposal: GroupUpdateRequest) {
        self.proposals.add_voting_proposal(proposal_id, proposal);
    }

    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: GroupUpdateRequest,
    ) {
        self.proposals.add_proposal(proposal_id, proposal);
    }

    pub fn clear_approved_proposals(&mut self) {
        self.proposals.clear_approved_proposals();
    }

    /// Get the epoch history (past batches of approved proposals).
    pub fn epoch_history(&self) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
        self.proposals.epoch_history()
    }
}
