//! Display helpers for rendering protobuf types in the UI.

use crate::mls_crypto::format_wallet_address;
use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, ViolationEvidence, ViolationType, app_message, group_update_request,
};

// ─────────────────────────── Member Role ───────────────────────────

/// A member's steward role at a given epoch.
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

/// String constants shared with the UI so string-keyed dispatch stays typo-safe.
pub mod message_types {
    pub const CONVERSATION_MESSAGE: &str = "ConversationMessage";
    pub const BAN_REQUEST: &str = "BanRequest";
    pub const PROPOSAL: &str = "Proposal";
    pub const VOTE: &str = "Vote";
    pub const VOTE_PAYLOAD: &str = "VotePayload";
    pub const USER_VOTE: &str = "UserVote";
    pub const PROPOSAL_ADDED: &str = "ProposalAdded";
    pub const COMMIT_CANDIDATE: &str = "CommitCandidate";
    pub const GROUP_SYNC: &str = "GroupSync";
    pub const UNKNOWN: &str = "Unknown";
}

/// Maps a protobuf message to a stable label string for the UI.
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
            app_message::Payload::GroupSync(_) => GROUP_SYNC,
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

// ─────────────────────────── Request Display ───────────────────────────

/// Wallet address for membership / emergency-evidence targets, or
/// `"epoch E | s1, s2, ..."` for elections (with `, retry R` appended
/// when `R > 0`). `"unknown"` otherwise.
pub fn format_group_request_target(request: &GroupUpdateRequest) -> String {
    match &request.payload {
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
            let meta = if se.retry_round == 0 {
                format!("epoch {}", se.election_epoch)
            } else {
                format!("epoch {}, retry {}", se.election_epoch, se.retry_round)
            };
            format!("{} | {}", meta, stewards.join(", "))
        }
        _ => "unknown".to_string(),
    }
}

/// `(action, target)` pair suitable for UI rendering.
pub fn format_group_request(request: &GroupUpdateRequest) -> (String, String) {
    (
        request.message_type().to_string(),
        format_group_request_target(request),
    )
}
