use std::collections::HashMap;

use crate::protos::de_mls::messages::v1::GroupUpdateRequest;

pub type ProposalId = u32;

#[derive(Clone, Debug, Default)]
pub struct CurrentEpochProposals {
    /// Proposals that have been approved and are waiting for the next voting epoch.
    approved_proposals: HashMap<ProposalId, GroupUpdateRequest>,

    /// Proposals that have been voted on and are waiting for the next voting epoch.
    voting_proposals: HashMap<ProposalId, GroupUpdateRequest>,
}

impl CurrentEpochProposals {
    /// Create a new steward with empty proposal queues.
    pub fn new() -> Self {
        Self {
            approved_proposals: HashMap::new(),
            voting_proposals: HashMap::new(),
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

    /// Clear the approved proposals after voting completes.
    pub fn clear_approved_proposals(&mut self) {
        self.approved_proposals.clear();
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
