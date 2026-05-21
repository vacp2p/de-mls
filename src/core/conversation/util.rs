//! Cross-cutting utilities used by Conversation, inbound dispatch, and
//! the app layer. Pure functions — no state, no I/O.

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use crate::protos::de_mls::messages::v1::{ConversationUpdateRequest, conversation_update_request};

/// Deterministic proposal ID for a self-leave, derived from the leaver's
/// identity. Pinning the ID dedupes a crash-retry against any in-flight
/// session via `ProposalAlreadyExist` and lets every node key the approved
/// entry under the same id.
pub fn self_leave_proposal_id(identity: &[u8]) -> u32 {
    let hash = Sha256::digest(identity);
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
}

/// True iff the `(proposal_id, request)` pair is an auto-approved self-leave
/// (identified by the deterministic ID signature).
pub(crate) fn is_auto_approved_entry(
    proposal_id: u32,
    request: &ConversationUpdateRequest,
) -> bool {
    match request.payload.as_ref() {
        Some(conversation_update_request::Payload::RemoveMember(r)) => {
            proposal_id == self_leave_proposal_id(&r.identity)
        }
        _ => false,
    }
}

/// Borrow-only `HashSet` view over a slice of identity blobs, for O(1)
/// membership lookups against `Vec<Vec<u8>>`.
pub fn member_set(members: &[Vec<u8>]) -> HashSet<&[u8]> {
    members.iter().map(|m| m.as_slice()).collect()
}

/// Return the target identity of a membership-changing `ConversationUpdateRequest`.
///
/// Used as the stable key for buffering pending updates so duplicates don't
/// stack when the same KP is re-broadcast. Returns `None` for non-membership
/// requests (emergency criteria, steward election).
pub fn target_identity_of(request: &ConversationUpdateRequest) -> Option<&[u8]> {
    match request.payload.as_ref()? {
        conversation_update_request::Payload::MemberInvite(m) => Some(&m.identity),
        conversation_update_request::Payload::RemoveMember(m) => Some(&m.identity),
        _ => None,
    }
}
