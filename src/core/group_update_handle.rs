//! Proposal lifecycle management for group membership changes.
//!
//! This module tracks proposals through their lifecycle:
//!
//! ```text
//! ┌─────────────┐    vote     ┌─────────────┐   commit   ┌─────────────┐
//! │   Voting    │ ──────────► │  Approved   │ ─────────► │  Archived   │
//! │  Proposals  │  (consensus)│  Proposals  │  (steward) │  (history)  │
//! └─────────────┘             └─────────────┘            └─────────────┘
//!        │
//!        │ (rejected)
//!        ▼
//!   ┌─────────┐
//!   │ Removed │
//!   └─────────┘
//! ```
//!
//! # Proposal Flow
//!
//! 1. **Voting**: Proposal created via `add_voting_proposal()`, waiting for votes
//! 2. **Approved**: Consensus reached, moved via `move_proposal_to_approved()`
//! 3. **Committed**: Steward batches proposals, clears via `clear_approved_proposals()`
//! 4. **Archived**: Past batches stored in `epoch_history` for UI display

use std::collections::{HashMap, VecDeque};

use crate::protos::de_mls::messages::v1::GroupUpdateRequest;

/// Consensus proposal identifier (assigned by the consensus service).
pub type ProposalId = u32;

/// Maximum number of past epoch batches to retain for UI display.
const MAX_EPOCH_HISTORY: usize = 10;

/// Tracks proposals through voting, approval, and commit lifecycle.
///
/// This is the internal state container for proposal management.
/// Use [`GroupHandle`](crate::core::GroupHandle) methods for access.
#[derive(Clone, Debug, Default)]
pub struct CurrentEpochProposals {
    /// Proposals waiting for consensus voting.
    /// Key: proposal_id from consensus service
    approved_proposals: HashMap<ProposalId, GroupUpdateRequest>,

    /// Proposals that passed consensus, waiting for steward to commit.
    voting_proposals: HashMap<ProposalId, GroupUpdateRequest>,

    /// History of committed proposal batches (most recent last).
    /// Limited to `MAX_EPOCH_HISTORY` entries for memory efficiency.
    epoch_history: VecDeque<HashMap<ProposalId, GroupUpdateRequest>>,
}

impl CurrentEpochProposals {
    /// Create a new steward with empty proposal queues.
    pub fn new() -> Self {
        Self {
            approved_proposals: HashMap::new(),
            voting_proposals: HashMap::new(),
            epoch_history: VecDeque::new(),
        }
    }

    /// Add a proposal to the approved proposals queue.
    pub fn add_proposal(&mut self, proposal_id: ProposalId, proposal: GroupUpdateRequest) {
        self.approved_proposals.insert(proposal_id, proposal);
    }

    /// Get the count of approved proposals waiting for voting.
    pub fn approved_proposals_count(&self) -> usize {
        self.approved_proposals.len()
    }

    /// Get a copy of the approved proposals.
    pub fn approved_proposals(&self) -> HashMap<ProposalId, GroupUpdateRequest> {
        self.approved_proposals.clone()
    }
    /// Add a proposal to the voting proposals queue.
    ///
    /// # Arguments
    /// * `proposal_id` - The proposal ID
    /// * `proposal` - The group update request to add
    pub fn add_voting_proposal(&mut self, proposal_id: ProposalId, proposal: GroupUpdateRequest) {
        self.voting_proposals.insert(proposal_id, proposal);
    }

    pub fn remove_voting_proposal(&mut self, proposal_id: ProposalId) {
        self.voting_proposals.remove(&proposal_id);
    }

    /// Clear the approved proposals, archiving them to epoch history.
    pub fn clear_approved_proposals(&mut self) {
        if !self.approved_proposals.is_empty() {
            let snapshot = std::mem::take(&mut self.approved_proposals);
            if self.epoch_history.len() >= MAX_EPOCH_HISTORY {
                self.epoch_history.pop_front();
            }
            self.epoch_history.push_back(snapshot);
        }
    }

    /// Get the epoch history (past batches of approved proposals, most recent last).
    pub fn epoch_history(&self) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
        &self.epoch_history
    }

    /// Remove a single proposal from the approved queue.
    ///
    /// Used for proposals that don't produce MLS operations (e.g., emergency criteria).
    pub fn remove_approved_proposal(&mut self, proposal_id: ProposalId) {
        self.approved_proposals.remove(&proposal_id);
    }

    pub fn move_proposal_to_approved(&mut self, proposal_id: ProposalId) {
        if let Some(proposal) = self.voting_proposals.remove(&proposal_id) {
            self.approved_proposals.insert(proposal_id, proposal);
        }
    }

    pub fn is_owner_of_proposal(&self, proposal_id: ProposalId) -> bool {
        self.voting_proposals.contains_key(&proposal_id)
    }
}
