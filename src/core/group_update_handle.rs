use std::collections::{HashMap, VecDeque};

use crate::protos::de_mls::messages::v1::GroupUpdateRequest;

pub type ProposalId = u32;

/// Maximum number of past epoch batches to retain for UI display.
const MAX_EPOCH_HISTORY: usize = 10;

#[derive(Clone, Debug, Default)]
pub struct CurrentEpochProposals {
    /// Proposals that have been approved and are waiting for the next voting epoch.
    approved_proposals: HashMap<ProposalId, GroupUpdateRequest>,

    /// Proposals that have been voted on and are waiting for the next voting epoch.
    voting_proposals: HashMap<ProposalId, GroupUpdateRequest>,

    /// History of approved proposal batches from past epochs (most recent last).
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

    /// Get the count of voting proposals.
    pub fn voting_proposals_count(&self) -> usize {
        self.voting_proposals.len()
    }

    /// Get a copy of the voting proposals.
    pub fn voting_proposals(&self) -> HashMap<ProposalId, GroupUpdateRequest> {
        self.voting_proposals.clone()
    }

    pub fn remove_voting_proposal(&mut self, proposal_id: ProposalId) {
        self.voting_proposals.remove(&proposal_id);
    }

    /// Clear the voting proposals after voting completes.
    pub fn clear_voting_proposals(&mut self) {
        self.voting_proposals.clear();
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

    pub fn move_proposal_to_approved(&mut self, proposal_id: ProposalId) {
        if let Some(proposal) = self.voting_proposals.remove(&proposal_id) {
            self.approved_proposals.insert(proposal_id, proposal);
        }
    }

    pub fn is_owner_of_proposal(&self, proposal_id: ProposalId) -> bool {
        self.voting_proposals.contains_key(&proposal_id)
    }
}
