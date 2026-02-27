//! App-layer message type labels for UI and gateway routing.

use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, ViolationType, app_message, group_update_request,
};

/// Message type constants for consistency and type safety in app/UI code.
pub mod message_types {
    pub const CONVERSATION_MESSAGE: &str = "ConversationMessage";
    pub const BAN_REQUEST: &str = "BanRequest";
    pub const PROPOSAL: &str = "Proposal";
    pub const VOTE: &str = "Vote";
    pub const VOTE_PAYLOAD: &str = "VotePayload";
    pub const USER_VOTE: &str = "UserVote";
    pub const PROPOSAL_ADDED: &str = "ProposalAdded";
    pub const COMMIT_CANDIDATE: &str = "CommitCandidate";
    pub const UNKNOWN: &str = "Unknown";
}

/// Trait for app-facing message type labels.
pub trait MessageType {
    fn message_type(&self) -> &'static str;
}

impl MessageType for app_message::Payload {
    fn message_type(&self) -> &'static str {
        use message_types::*;
        match self {
            app_message::Payload::ConversationMessage(_) => CONVERSATION_MESSAGE,
            app_message::Payload::BanRequest(_) => BAN_REQUEST,
            app_message::Payload::Proposal(_) => PROPOSAL,
            app_message::Payload::Vote(_) => VOTE,
            app_message::Payload::VotePayload(_) => VOTE_PAYLOAD,
            app_message::Payload::UserVote(_) => USER_VOTE,
            app_message::Payload::ProposalAdded(_) => PROPOSAL_ADDED,
            app_message::Payload::CommitCandidate(_) => COMMIT_CANDIDATE,
        }
    }
}

impl MessageType for GroupUpdateRequest {
    fn message_type(&self) -> &'static str {
        match &self.payload {
            Some(group_update_request::Payload::InviteMember(_)) => "Add Member",
            Some(group_update_request::Payload::RemoveMember(_)) => "Remove Member",
            Some(group_update_request::Payload::EmergencyCriteria(ec)) => ec
                .evidence
                .as_ref()
                .map(|e| match ViolationType::try_from(e.violation_type) {
                    Ok(ViolationType::BrokenCommit) => "Emergency: Broken Commit",
                    Ok(ViolationType::BrokenMlsProposal) => "Emergency: Broken MLS Proposal",
                    Ok(ViolationType::CensorshipInactivity) => "Emergency: Censorship/Inactivity",
                    _ => "Emergency: Unknown Violation",
                })
                .unwrap_or("Emergency: Unknown Violation"),
            _ => "Unknown",
        }
    }
}
