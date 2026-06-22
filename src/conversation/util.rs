//! Cross-cutting utilities used by Conversation, inbound dispatch, and the app layer.

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use crate::protos::de_mls::messages::v1::{ConversationUpdateRequest, conversation_update_request};

/// Deterministic proposal ID for a self-leave, derived from the leaver's ID.
/// Pinning the ID dedupes a crash-retry against any in-flight
/// session via `ProposalAlreadyExist` and lets every node key the approved
/// entry under the same id.
pub fn self_leave_proposal_id(member_id: &[u8]) -> u32 {
    let hash = Sha256::digest(member_id);
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
}

/// True iff the `(proposal_id, request)` pair is an auto-approved self-leave
/// (identified by the deterministic ID signature).
pub fn is_auto_approved_entry(proposal_id: u32, request: &ConversationUpdateRequest) -> bool {
    match request.payload.as_ref() {
        Some(conversation_update_request::Payload::RemoveMember(r)) => {
            proposal_id == self_leave_proposal_id(&r.member_id)
        }
        _ => false,
    }
}

/// Borrow-only `HashSet` view over a slice of member_id blobs, for O(1)
/// membership lookups against `Vec<Vec<u8>>`.
pub fn member_set(members: &[Vec<u8>]) -> HashSet<&[u8]> {
    members.iter().map(|m| m.as_slice()).collect()
}

/// Return the target member_id of a membership-changing `ConversationUpdateRequest`.
pub fn target_member_id_of(request: &ConversationUpdateRequest) -> Option<&[u8]> {
    match request.payload.as_ref()? {
        conversation_update_request::Payload::MemberInvite(m) => Some(&m.member_id),
        conversation_update_request::Payload::RemoveMember(m) => Some(&m.member_id),
        _ => None,
    }
}
