//! Display helpers for converting protobuf types to human-readable formats.
//!
//! These functions and traits are convenience utilities for UI display.
//! They are not required for protocol operation.

use prost::Message;

use crate::mls_crypto::format_wallet_address;
use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, ViolationEvidence, ViolationType, app_message, group_update_request,
};

// ─────────────────────────── Member Role ───────────────────────────

/// A member's steward role within a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    /// The designated steward for the current epoch.
    EpochSteward,
    /// The backup steward for the current epoch.
    BackupSteward,
    /// On the steward list, but not epoch or backup steward.
    Steward,
    /// Not on the steward list.
    Member,
}

impl std::fmt::Display for MemberRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemberRole::EpochSteward => write!(f, "epoch_steward"),
            MemberRole::BackupSteward => write!(f, "backup_steward"),
            MemberRole::Steward => write!(f, "steward"),
            MemberRole::Member => write!(f, "member"),
        }
    }
}

// ─────────────────────────── Message Type Labels ───────────────────────────

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
    pub const STEWARD_LIST_SYNC: &str = "StewardListSync";
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
            app_message::Payload::StewardListSync(_) => STEWARD_LIST_SYNC,
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
                    Ok(ViolationType::ScoreBelowThreshold) => "Emergency: Score Below Threshold",
                    _ => "Emergency: Unknown Violation",
                })
                .unwrap_or("Emergency: Unknown Violation"),
            Some(group_update_request::Payload::StewardElection(_)) => "Steward Election",
            _ => "Unknown",
        }
    }
}

// ─────────────────────────── Violation Labels ───────────────────────────

impl ViolationEvidence {
    /// Human-readable label for the violation type (used in display / UI).
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

// ─────────────────────────── Request Display ───────────────────────────

/// Convert a serialized `GroupUpdateRequest` to a display-friendly `(action, target)` pair.
pub fn convert_group_request_to_display(request: Vec<u8>) -> (String, String) {
    let request = GroupUpdateRequest::decode(request.as_slice()).unwrap_or_default();
    let action = request.message_type().to_string();
    let target = match &request.payload {
        Some(group_update_request::Payload::InviteMember(im)) => {
            format_wallet_address(&im.identity)
        }
        Some(group_update_request::Payload::RemoveMember(rm)) => {
            format_wallet_address(&rm.identity)
        }
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => ec
            .evidence
            .as_ref()
            .map(|e| format_wallet_address(&e.target_member_id))
            .unwrap_or_else(|| "unknown".to_string()),
        Some(group_update_request::Payload::StewardElection(se)) => {
            let stewards: Vec<String> = se
                .proposed_stewards
                .iter()
                .map(|s| format_wallet_address(s))
                .collect();
            format!("epoch {} | {}", se.election_epoch, stewards.join(", "))
        }
        _ => "Invalid request".to_string(),
    };
    (action, target)
}

/// Extract the identity string from a `GroupUpdateRequest`.
pub fn get_identity_from_group_update_request(req: GroupUpdateRequest) -> String {
    match req.payload {
        Some(group_update_request::Payload::InviteMember(im)) => {
            format_wallet_address(&im.identity)
        }
        Some(group_update_request::Payload::RemoveMember(rm)) => {
            format_wallet_address(&rm.identity)
        }
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => ec
            .evidence
            .as_ref()
            .map(|e| format_wallet_address(&e.target_member_id))
            .unwrap_or_else(|| "unknown".to_string()),
        Some(group_update_request::Payload::StewardElection(se)) => {
            let stewards: Vec<String> = se
                .proposed_stewards
                .iter()
                .map(|s| format_wallet_address(s))
                .collect();
            format!("epoch {} | {}", se.election_epoch, stewards.join(", "))
        }
        _ => "unknown".to_string(),
    }
}
