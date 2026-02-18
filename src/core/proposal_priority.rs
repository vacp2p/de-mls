//! Proposal priority for consensus proposals.
//!
//! Emergency criteria proposals have the highest priority. When a higher-priority
//! proposal is active and not yet finalized, lower-priority proposals should be
//! deferred.

use crate::protos::de_mls::messages::v1::{GroupUpdateRequest, group_update_request};

/// Priority levels for consensus proposals (higher value = higher priority).
///
/// When a proposal of a given priority is active, proposals of
/// strictly lower priority should be dropped or deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProposalPriority {
    /// Commit proposals (add/remove member) — lowest priority.
    Commit = 0,
    /// Emergency criteria proposals — highest priority.
    Emergency = 2,
}

impl ProposalPriority {
    /// Determine the priority of a `GroupUpdateRequest` based on its payload.
    pub fn from_request(request: &GroupUpdateRequest) -> Self {
        match &request.payload {
            Some(group_update_request::Payload::EmergencyCriteria(_)) => {
                ProposalPriority::Emergency
            }
            _ => ProposalPriority::Commit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::de_mls::messages::v1::{InviteMember, RemoveMember, ViolationEvidence};

    #[test]
    fn test_invite_member_is_commit_priority() {
        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                key_package_bytes: vec![],
                identity: vec![],
            })),
        };
        assert_eq!(
            ProposalPriority::from_request(&request),
            ProposalPriority::Commit
        );
    }

    #[test]
    fn test_remove_member_is_commit_priority() {
        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: vec![],
            })),
        };
        assert_eq!(
            ProposalPriority::from_request(&request),
            ProposalPriority::Commit
        );
    }

    #[test]
    fn test_emergency_criteria_is_emergency_priority() {
        let request =
            ViolationEvidence::broken_commit(vec![], 0, Vec::<u8>::new()).into_update_request();
        assert_eq!(
            ProposalPriority::from_request(&request),
            ProposalPriority::Emergency
        );
    }

    #[test]
    fn test_emergency_is_higher_than_commit() {
        assert!(ProposalPriority::Emergency > ProposalPriority::Commit);
    }
}
