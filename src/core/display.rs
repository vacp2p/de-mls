//! Display helpers for converting protobuf types to human-readable formats.
//!
//! These functions are convenience utilities for UI display. They are not
//! required for protocol operation.

use prost::Message;

use crate::mls_crypto::format_wallet_address;
use crate::protos::de_mls::messages::v1::{GroupUpdateRequest, group_update_request};

/// Convert a serialized `GroupUpdateRequest` to a display-friendly `(action, target)` pair.
pub fn convert_group_request_to_display(request: Vec<u8>) -> (String, String) {
    let request = GroupUpdateRequest::decode(request.as_slice()).unwrap_or_default();
    match request.payload {
        Some(group_update_request::Payload::InviteMember(im)) => (
            "Add Member".to_string(),
            format_wallet_address(&im.identity),
        ),
        Some(group_update_request::Payload::RemoveMember(rm)) => (
            "Remove Member".to_string(),
            format_wallet_address(&rm.identity),
        ),
        Some(group_update_request::Payload::EmergencyCriteria(ec)) => {
            let (label, target) = match ec.evidence.as_ref() {
                Some(e) => (
                    format!("Emergency: {}", e.violation_type_label()),
                    format_wallet_address(&e.target_member_id),
                ),
                None => (
                    "Emergency: Unknown Violation".to_string(),
                    "unknown".to_string(),
                ),
            };
            (label, target)
        }
        _ => ("Unknown".to_string(), "Invalid request".to_string()),
    }
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
