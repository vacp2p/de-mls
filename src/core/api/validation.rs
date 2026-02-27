use super::*;

// ─────────────────────────── Batch Validation ───────────────────────────

/// Validate MLS actions from a commit against local voted proposals.
///
/// Returns `Some(ProcessResult::ViolationDetected(...))` if a violation is found,
/// `None` if all checks pass.
pub(crate) fn validate_commit_candidate(
    handle: &GroupHandle,
    local_proposals: &HashMap<ProposalId, GroupUpdateRequest>,
    sender_id: &[u8],
    mls_actions: &[MlsProposalAction],
) -> Result<Option<ProcessResult>, CoreError> {
    let group_name = handle.group_name();

    let mut expected_actions: Vec<MlsProposalAction> = local_proposals
        .values()
        .filter_map(expected_action_for_request)
        .collect();
    let mut actual_actions = mls_actions.to_vec();

    expected_actions.sort();
    actual_actions.sort();

    if actual_actions != expected_actions {
        tracing::warn!(
            "Violation: broken MLS proposal for group {} — \
             MLS actions {:?} don't match voted {:?}",
            group_name,
            actual_actions,
            expected_actions,
        );
        return Ok(Some(ProcessResult::ViolationDetected(
            ViolationEvidence::broken_mls_proposal(
                sender_id.to_vec(),
                handle.current_epoch(),
                format!("MLS actions {actual_actions:?} != voted {expected_actions:?}"),
            ),
        )));
    }

    Ok(None)
}

/// Derive the expected [`MlsProposalAction`] from a voted [`GroupUpdateRequest`].
fn expected_action_for_request(req: &GroupUpdateRequest) -> Option<MlsProposalAction> {
    use crate::protos::de_mls::messages::v1::group_update_request::Payload;
    match &req.payload {
        Some(Payload::InviteMember(im)) => Some(MlsProposalAction::Add(im.identity.clone())),
        Some(Payload::RemoveMember(rm)) => Some(MlsProposalAction::Remove(rm.identity.clone())),
        // Emergency criteria proposals don't produce MLS proposals
        Some(Payload::EmergencyCriteria(_)) | None => None,
    }
}
