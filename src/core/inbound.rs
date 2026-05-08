//! Inbound app-subtopic message processing.
//!
//! Welcome-subtopic packets are handled at the app layer (because the
//! invitation path constructs a new `MlsService` via the user-supplied
//! factory) and are not routed through this module.

use hashgraph_like_consensus::protos::consensus::v1::Proposal;
use prost::Message;
use tracing::{info, warn};

use crate::{
    core::{
        error::CoreError, freeze::process_commit_candidate, group::Group,
        process_result::ProcessResult,
    },
    identity::{ShortId, parse_wallet_to_bytes},
    mls_crypto::{DecryptResult, MlsService},
    protos::de_mls::messages::v1::{
        AppMessage, GroupUpdateRequest, app_message, group_update_request,
    },
};

/// Fast-path proposals (`expected_voters_count == 1`) bypass peer voting, so
/// we restrict them to self-removal. Enforcing that the MLS-authenticated
/// sender matches the `RemoveMember` target closes the unilateral-removal
/// vector that an otherwise-free `expected_voters == 1` opens.
///
/// Returns `true` when the proposal is allowed. A mismatch (different
/// target, wrong payload variant, or undecodable) produces `false`.
fn authorize_fast_path_proposal(proposal: &Proposal, mls_sender: &[u8]) -> bool {
    if proposal.expected_voters_count != 1 {
        return true;
    }
    if proposal.proposal_owner != mls_sender {
        return false;
    }
    let Ok(request) = GroupUpdateRequest::decode(proposal.payload.as_slice()) else {
        return false;
    };
    matches!(
        request.payload,
        Some(group_update_request::Payload::RemoveMember(ref r)) if r.identity == mls_sender
    )
}

/// Process an inbound packet on the app subtopic and decide what action is
/// needed. Welcome-subtopic packets are handled at the app layer.
pub fn process_inbound<M: MlsService>(
    group: &mut Group,
    mls: &M,
    payload: &[u8],
) -> Result<ProcessResult, CoreError> {
    // 1. Try plaintext CommitCandidate (sent as plaintext AppMessage)
    if let Ok(app_message) = AppMessage::decode(payload) {
        if let Some(app_message::Payload::CommitCandidate(candidate)) = app_message.payload {
            return process_commit_candidate(group, mls, candidate);
        }
    }

    // 2. MLS-encrypted app messages only — use decrypt_application_only.
    //    This NEVER stores proposals or processes commits, preventing
    //    rogue MLS proposals on the app subtopic from polluting state.
    let res = mls.decrypt_application_only(payload)?;

    match res {
        DecryptResult::Application(app_bytes, sender) => {
            let app_msg = AppMessage::decode(app_bytes.as_ref())?;
            if let Some(app_message::Payload::Proposal(proposal)) = &app_msg.payload
                && !authorize_fast_path_proposal(proposal, &sender)
            {
                warn!(
                    group = group.group_name(),
                    proposal_id = proposal.proposal_id,
                    sender = %ShortId(&sender),
                    owner = %ShortId(&proposal.proposal_owner),
                    "fast-path proposal rejected: sender is not the self-removal target"
                );
                return Ok(ProcessResult::Noop);
            }
            // Drop BanRequests whose target isn't in the group — saves a
            // useless consensus round.
            if let Some(app_message::Payload::BanRequest(ban)) = &app_msg.payload {
                let target = parse_wallet_to_bytes(&ban.user_to_ban)?;
                if !mls.is_member(&target) {
                    info!(
                        group = group.group_name(),
                        target = %ShortId(&target),
                        "ban request skipped: target not a member"
                    );
                    return Ok(ProcessResult::Noop);
                }
            }
            app_msg.try_into()
        }
        DecryptResult::Removed(_) => Ok(ProcessResult::LeaveGroup),
        DecryptResult::Ignored => {
            tracing::debug!(
                group = group.group_name(),
                "app message ignored (wrong epoch/group)"
            );
            Ok(ProcessResult::Noop)
        }
        _ => {
            warn!(
                group = group.group_name(),
                "unexpected MLS message type on app subtopic"
            );
            Ok(ProcessResult::Noop)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::group::self_leave_proposal_id;
    use crate::protos::de_mls::messages::v1::RemoveMember;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn remove_payload(identity: &[u8]) -> Vec<u8> {
        GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: identity.to_vec(),
            })),
        }
        .encode_to_vec()
    }

    fn proposal_for_self_remove(sender: &[u8], expected_voters: u32) -> Proposal {
        Proposal {
            name: "test".into(),
            payload: remove_payload(sender),
            proposal_id: self_leave_proposal_id(sender),
            proposal_owner: sender.to_vec(),
            votes: Vec::new(),
            expected_voters_count: expected_voters,
            round: 1,
            timestamp: 0,
            expiration_timestamp: u64::MAX,
            liveness_criteria_yes: true,
        }
    }

    #[test]
    fn fast_path_allows_self_removal_matching_sender() {
        let sender = member(1);
        let proposal = proposal_for_self_remove(&sender, 1);
        assert!(authorize_fast_path_proposal(&proposal, &sender));
    }

    #[test]
    fn fast_path_rejects_target_other_than_sender() {
        let sender = member(1);
        let victim = member(2);
        let mut proposal = proposal_for_self_remove(&victim, 1);
        proposal.proposal_owner = sender.clone();
        assert!(!authorize_fast_path_proposal(&proposal, &sender));
    }

    #[test]
    fn fast_path_rejects_owner_mismatch() {
        let sender = member(1);
        let imposter = member(3);
        let mut proposal = proposal_for_self_remove(&sender, 1);
        proposal.proposal_owner = imposter;
        assert!(!authorize_fast_path_proposal(&proposal, &sender));
    }

    #[test]
    fn fast_path_rejects_non_remove_payload() {
        let sender = member(1);
        let mut proposal = proposal_for_self_remove(&sender, 1);
        proposal.payload = vec![0xff; 8]; // garbage
        assert!(!authorize_fast_path_proposal(&proposal, &sender));
    }

    #[test]
    fn expected_voters_gt_one_bypasses_authz() {
        let sender = member(1);
        let victim = member(2);
        let mut proposal = proposal_for_self_remove(&victim, 5);
        proposal.proposal_owner = sender.clone();
        // Regular proposals (voters > 1) aren't gated by this check — peer
        // voting provides the authorization instead.
        assert!(authorize_fast_path_proposal(&proposal, &sender));
    }
}
