//! Display helpers for converting protobuf types to human-readable formats.
//!
//! These functions are convenience utilities for UI display. They are not
//! required for protocol operation.

use prost::Message;

use crate::app::message_type::MessageType;
use crate::mls_crypto::format_wallet_address;
use crate::protos::de_mls::messages::v1::{
    GroupUpdateRequest, ViolationEvidence, ViolationType, group_update_request,
};

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
        _ => "unknown".to_string(),
    }
}
