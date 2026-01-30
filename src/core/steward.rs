//! Simplified steward for proposal management.
//!
//! This steward implementation handles proposal collection and management
//! without any cryptographic operations. Key package encryption/decryption
//! is handled at the application layer.

use super::types::GroupUpdateRequest;

/// A steward manages proposals for a group during voting epochs.
///
/// The steward collects proposals (add/remove member requests) and manages
/// them through the voting lifecycle:
///
/// 1. Proposals are added to `approved_proposals` as they arrive
/// 2. When voting starts, proposals move to `voting_proposals`
/// 3. After voting completes, `voting_proposals` are cleared
#[derive(Clone, Debug, Default)]
pub struct Steward {
    /// Proposals that have been approved and are waiting for the next voting epoch.
    approved_proposals: Vec<GroupUpdateRequest>,
    /// Proposals currently being voted on.
    voting_proposals: Vec<GroupUpdateRequest>,
}

impl Steward {
    /// Create a new steward with empty proposal queues.
    pub fn new() -> Self {
        Self {
            approved_proposals: Vec::new(),
            voting_proposals: Vec::new(),
        }
    }

    /// Add a proposal to the approved proposals queue.
    ///
    /// # Arguments
    /// * `proposal` - The group update request to add
    pub fn add_proposal(&mut self, proposal: GroupUpdateRequest) {
        self.approved_proposals.push(proposal);
    }

    /// Get the count of approved proposals waiting for voting.
    pub fn approved_proposals_count(&self) -> usize {
        self.approved_proposals.len()
    }

    /// Get a copy of the approved proposals.
    pub fn approved_proposals(&self) -> Vec<GroupUpdateRequest> {
        self.approved_proposals.clone()
    }

    /// Start a new voting epoch by moving approved proposals to voting proposals.
    ///
    /// # Returns
    /// The number of proposals moved to the voting epoch.
    pub fn start_voting_epoch(&mut self) -> usize {
        let proposals: Vec<_> = self.approved_proposals.drain(..).collect();
        let count = proposals.len();
        self.voting_proposals.extend(proposals);
        count
    }

    /// Get the count of proposals in the current voting epoch.
    pub fn voting_proposals_count(&self) -> usize {
        self.voting_proposals.len()
    }

    /// Get a copy of the proposals in the current voting epoch.
    pub fn voting_proposals(&self) -> Vec<GroupUpdateRequest> {
        self.voting_proposals.clone()
    }

    /// Clear the voting proposals after voting completes.
    pub fn complete_voting(&mut self) {
        self.voting_proposals.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_steward_proposal_lifecycle() {
        let mut steward = Steward::new();

        // Initially empty
        assert_eq!(steward.approved_proposals_count(), 0);
        assert_eq!(steward.voting_proposals_count(), 0);

        // Add some proposals
        steward.add_proposal(GroupUpdateRequest::RemoveMember(
            "0x1234567890123456789012345678901234567890".to_string(),
        ));
        steward.add_proposal(GroupUpdateRequest::RemoveMember(
            "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".to_string(),
        ));

        assert_eq!(steward.approved_proposals_count(), 2);
        assert_eq!(steward.voting_proposals_count(), 0);

        // Start voting epoch
        let count = steward.start_voting_epoch();
        assert_eq!(count, 2);
        assert_eq!(steward.approved_proposals_count(), 0);
        assert_eq!(steward.voting_proposals_count(), 2);

        // Complete voting
        steward.complete_voting();
        assert_eq!(steward.voting_proposals_count(), 0);
    }
}
