//! Cross-cutting utilities used by Conversation, inbound dispatch, and the app layer.

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use crate::mls_crypto::signature_key_of_key_package;
use crate::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, ViolationType, conversation_update_request,
};

/// Deterministic proposal ID for a self-leave, derived from the leaver's ID.
/// Pinning the ID dedupes a crash-retry against any in-flight
/// session via `ProposalAlreadyExist` and lets every node key the approved
/// entry under the same id.
pub fn self_leave_proposal_id(member_id: &[u8]) -> u32 {
    let hash = Sha256::digest(member_id);
    u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]])
}

/// Borrow-only `HashSet` view over a slice of member_id blobs, for O(1)
/// membership lookups against `Vec<Vec<u8>>`.
pub fn member_set(members: &[Vec<u8>]) -> HashSet<&[u8]> {
    members.iter().map(|m| m.as_slice()).collect()
}

/// The target of a membership-changing `ConversationUpdateRequest`: a removal's
/// `member_id` (leaf index), or an invite's joiner signature key — read from the
/// key package, since the invite carries only that. `None` for other payloads,
/// or an unparseable key package.
pub fn target_member_id_of(request: &ConversationUpdateRequest) -> Option<Vec<u8>> {
    match request.payload.as_ref()? {
        conversation_update_request::Payload::MemberInvite(m) => {
            signature_key_of_key_package(&m.key_package_bytes).ok()
        }
        conversation_update_request::Payload::RemoveMember(m) => Some(m.member_id.clone()),
        _ => None,
    }
}

/// Member a proposal will add or remove once it lands — the membership target,
/// plus a score-below-threshold ECP, which transforms into a RemoveMember on
/// approval. Nothing in de-mls raises that ECP; an integrator acting on a low
/// score can. Covering it here lets the in-flight index dedup a repeated or
/// buffered change against one for its whole voting window, not just after it
/// becomes a RemoveMember.
pub fn in_flight_target(request: &ConversationUpdateRequest) -> Option<Vec<u8>> {
    if let Some(id) = target_member_id_of(request) {
        return Some(id);
    }
    let conversation_update_request::Payload::EmergencyCriteria(ec) = request.payload.as_ref()?
    else {
        return None;
    };
    let ev = ec.evidence.as_ref()?;
    (ViolationType::try_from(ev.violation_type) == Ok(ViolationType::ScoreBelowThreshold))
        .then(|| ev.target_member_id.clone())
}
