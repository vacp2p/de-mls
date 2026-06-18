use std::fmt;

use crate::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, ViolationEvidence, ViolationType, app_message,
    conversation_update_request,
};

// ─────────────────────────── Member Role ───────────────────────────

/// A member's steward-list role for a given epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    /// Epoch steward for this epoch.
    EpochSteward,
    /// Backup steward for this epoch.
    BackupSteward,
    /// On the steward list but not in the epoch or backup slot.
    Steward,
    /// Not on the steward list.
    Member,
}

impl fmt::Display for MemberRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemberRole::EpochSteward => write!(f, "epoch_steward"),
            MemberRole::BackupSteward => write!(f, "backup_steward"),
            MemberRole::Steward => write!(f, "steward"),
            MemberRole::Member => write!(f, "member"),
        }
    }
}

// ─────────────────────────── Message Type Labels ───────────────────────────

/// String constants shared with the UI so string-keyed dispatch stays typo-safe.
pub mod message_types {
    pub const CONVERSATION_MESSAGE: &str = "ConversationMessage";
    pub const MEMBERSHIP_CHANGE: &str = "MembershipChange";
    pub const BAN_REQUEST: &str = "BanRequest";
    pub const PROPOSAL: &str = "Proposal";
    pub const VOTE: &str = "Vote";
    pub const VOTE_PAYLOAD: &str = "VotePayload";
    pub const USER_VOTE: &str = "UserVote";
    pub const PROPOSAL_ADDED: &str = "ProposalAdded";
    pub const COMMIT_CANDIDATE: &str = "CommitCandidate";
    pub const CONVERSATION_SYNC: &str = "ConversationSync";
    pub const MEMBER_WELCOME: &str = "MemberWelcome";
    pub const UNKNOWN: &str = "Unknown";
}

/// Maps a protobuf message to a stable label string for the UI.
pub trait MessageType {
    fn message_type(&self) -> &'static str;
}

impl MessageType for app_message::Payload {
    fn message_type(&self) -> &'static str {
        match self {
            app_message::Payload::ConversationMessage(_) => message_types::CONVERSATION_MESSAGE,
            app_message::Payload::MembershipChange(_) => message_types::MEMBERSHIP_CHANGE,
            app_message::Payload::BanRequest(_) => message_types::BAN_REQUEST,
            app_message::Payload::Proposal(_) => message_types::PROPOSAL,
            app_message::Payload::Vote(_) => message_types::VOTE,
            app_message::Payload::VotePayload(_) => message_types::VOTE_PAYLOAD,
            app_message::Payload::UserVote(_) => message_types::USER_VOTE,
            app_message::Payload::ProposalAdded(_) => message_types::PROPOSAL_ADDED,
            app_message::Payload::CommitCandidate(_) => message_types::COMMIT_CANDIDATE,
            app_message::Payload::ConversationSync(_) => message_types::CONVERSATION_SYNC,
            app_message::Payload::MemberWelcome(_) => message_types::MEMBER_WELCOME,
        }
    }
}

impl MessageType for ConversationUpdateRequest {
    fn message_type(&self) -> &'static str {
        match &self.payload {
            Some(conversation_update_request::Payload::MemberInvite(_)) => "Add Member",
            Some(conversation_update_request::Payload::RemoveMember(_)) => "Remove Member",
            Some(conversation_update_request::Payload::EmergencyCriteria(ec)) => ec
                .evidence
                .as_ref()
                .map(|e| match ViolationType::try_from(e.violation_type) {
                    Ok(ViolationType::BrokenCommit) => "Emergency: Broken Commit",
                    Ok(ViolationType::BrokenMlsProposal) => "Emergency: Broken MLS Proposal",
                    Ok(ViolationType::CensorshipInactivity) => "Emergency: Censorship/Inactivity",
                    Ok(ViolationType::ScoreBelowThreshold) => "Emergency: Score Below Threshold",
                    _ => "Emergency: Unknown Violation",
                })
                .unwrap_or("Emergency: Unknown Violation"),
            Some(conversation_update_request::Payload::StewardElection(_)) => "Steward Election",
            _ => "Unknown",
        }
    }
}

// ─────────────────────────── Violation Labels ───────────────────────────

impl ViolationEvidence {
    /// Human-readable label for the violation type.
    pub fn violation_type_label(&self) -> &'static str {
        match ViolationType::try_from(self.violation_type) {
            Ok(ViolationType::BrokenCommit) => "Broken Commit",
            Ok(ViolationType::BrokenMlsProposal) => "Broken MLS Proposal",
            Ok(ViolationType::CensorshipInactivity) => "Censorship/Inactivity",
            Ok(ViolationType::ScoreBelowThreshold) => "Score Below Threshold",
            _ => "Unknown Violation",
        }
    }
}
