//! The pure half of outcome handling: given a resolved proposal's decision
//! and the request it carried, [`apply_outcome`] reshapes [`EngineQueues`]
//! and names the follow-up the acting half in the parent module still owes.

use tracing::info;

use crate::{
    engine::{
        proposal_kind::change_of,
        queues::{Admission, EngineQueues},
        types::{ActionKind, Verdict},
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, ViolationEvidence, ViolationType,
        conversation_update_request,
    },
};

/// What [`apply_outcome`] decided. The queue changes are already done; each
/// variant names the follow-up still owed — install an election, skip a
/// silent epoch steward, commit a removal, and so on.
#[derive(Debug, Clone)]
pub(crate) enum ApplyOutcome {
    /// Nothing left to do. "No follow-up", not "no change" — some of these
    /// paths still adjusted the approved queue.
    NoAction,
    /// The election passed. Validate the proposed list and install it.
    ElectionAccepted(StewardElectionProposal),
    /// The election did not pass — NO, or no decision at the timeout.
    /// Nothing automatic follows.
    ElectionDropped,
    /// A `Deadlock` proposal passed: skip the current epoch steward for this
    /// epoch so the next eligible steward on the list becomes ES.
    DeadlockAccepted,
    /// A below-threshold removal passed. The urgent-commit target is already
    /// set, so commit it now rather than waiting out the inactivity timer,
    /// and refresh the steward list if `target` was a steward.
    UrgentRemoval { target: Vec<u8> },
    /// A regular `RemoveMember` passed and is queued for the next commit.
    /// Refresh the steward list if `target` was a steward.
    QueuedRemoval { target: Vec<u8> },
}

/// Apply a verdict. An approved membership change is offered to the queue
/// through [`EngineQueues::admit_approved`]; an approved below-threshold
/// emergency becomes a `RemoveMember` and an urgent commit; an approved
/// Deadlock skips the epoch steward; an approved election is handed back
/// to install. A rejected or failed proposal is dropped. An emergency's
/// partial freeze lifts whatever the verdict.
pub(crate) fn apply_outcome(
    queues: &mut EngineQueues,
    members: &[Vec<u8>],
    proposal_id: u32,
    verdict: Verdict,
    request: &ConversationUpdateRequest,
) -> ApplyOutcome {
    // The session is over, so the proposal leaves the voting index. That
    // also ends the partial freeze an emergency held, since the freeze is
    // "an emergency is in the index". On YES the proposal re-enters below
    // as an approved entry.
    queues.remove_voting_proposal(proposal_id);

    if let Some(election) = extract_election_proposal(request).cloned() {
        return apply_election_result(verdict, election);
    }

    // ── Emergency and regular proposals ──

    let evidence = extract_emergency_evidence(request).cloned();
    let is_emergency = evidence.is_some();

    // Queue effects follow from approval alone.
    let approved = verdict == Verdict::Approved;

    // Should the approved ECP transform into a RemoveMember?
    let transforms_to_removal =
        approved && is_emergency && evidence.as_ref().is_some_and(is_score_below_threshold);

    // Target for approved-queue dedup and downstream steward-list refresh.
    let removal_target = pending_removal_target(
        request,
        evidence.as_ref(),
        approved,
        is_emergency,
        transforms_to_removal,
    );

    if let Some(ev) = evidence.as_ref() {
        if approved {
            info!(
                proposal_id,
                target = ?ev.target_member_id,
                creator = ?ev.creator_member_id,
                "emergency criteria proposal accepted"
            );
        } else {
            info!(
                proposal_id,
                creator = ?ev.creator_member_id,
                verdict = ?verdict,
                "emergency criteria proposal dropped"
            );
        }
    }

    // A below-threshold ECP becomes a `RemoveMember`, and the next commit is
    // restricted to that target so the removal doesn't drag along unrelated
    // approved work. `transforms_to_removal` implies `approved` and `evidence`
    // is `Some`; `filter` binds it without an unwrap. Other emergencies
    // produce no membership change, so there is nothing to queue.
    let transformed = evidence.as_ref().filter(|_| transforms_to_removal);
    let to_queue = match transformed {
        Some(ev) => Some(removal_request_for(ev)),
        None => (approved && !is_emergency).then(|| request.clone()),
    };
    if let Some(queued) = to_queue {
        match queues.admit_approved(members, proposal_id, queued) {
            Admission::Duplicate => {
                info!(
                    proposal_id,
                    "approval deduped — the change is already queued"
                );
                return ApplyOutcome::NoAction;
            }
            Admission::Stale => {
                info!(
                    proposal_id,
                    "approval dropped: stale, its change is in effect or its owner is being removed"
                );
                return ApplyOutcome::NoAction;
            }
            Admission::Queued => {}
        }
    }
    if let Some(ev) = transformed {
        let target = ev.target_member_id.clone();
        queues.set_urgent_commit_target(target.clone());
        return ApplyOutcome::UrgentRemoval { target };
    }

    if evidence.as_ref().is_some_and(is_deadlock) && approved {
        return ApplyOutcome::DeadlockAccepted;
    }
    if let Some(target) = removal_target {
        return ApplyOutcome::QueuedRemoval { target };
    }
    ApplyOutcome::NoAction
}

/// The election branch of [`apply_outcome`]. YES hands the proposed steward
/// list back for validation and install; NO or a failed session just reports
/// the drop.
fn apply_election_result(verdict: Verdict, election: StewardElectionProposal) -> ApplyOutcome {
    if verdict == Verdict::Approved {
        info!(
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            "steward election proposal accepted"
        );
        ApplyOutcome::ElectionAccepted(election)
    } else {
        info!(?verdict, "steward election proposal dropped");
        ApplyOutcome::ElectionDropped
    }
}

/// Emergency evidence carried by a request, if any.
fn extract_emergency_evidence(req: &ConversationUpdateRequest) -> Option<&ViolationEvidence> {
    match &req.payload {
        Some(conversation_update_request::Payload::EmergencyCriteria(ec)) => ec.evidence.as_ref(),
        _ => None,
    }
}

/// The steward election carried by a request, if any.
fn extract_election_proposal(req: &ConversationUpdateRequest) -> Option<&StewardElectionProposal> {
    match &req.payload {
        Some(conversation_update_request::Payload::StewardElection(se)) => Some(se),
        _ => None,
    }
}

/// Whether evidence is a score-below-threshold violation.
fn is_score_below_threshold(evidence: &ViolationEvidence) -> bool {
    ViolationType::try_from(evidence.violation_type) == Ok(ViolationType::ScoreBelowThreshold)
}

/// Whether evidence is the layer-3 anti-deadlock signal.
fn is_deadlock(evidence: &ViolationEvidence) -> bool {
    ViolationType::try_from(evidence.violation_type) == Ok(ViolationType::Deadlock)
}

/// The `RemoveMember` request a score-below-threshold approval becomes.
fn removal_request_for(evidence: &ViolationEvidence) -> ConversationUpdateRequest {
    ConversationUpdateRequest::remove_member(evidence.target_member_id.clone())
}

/// The member this approval would queue for removal, if any. Covers a direct
/// `RemoveMember` and a score-below-threshold ECP that transforms into one.
/// `None` for elections, non-removal emergencies, non-removal regular
/// proposals, and rejections.
fn pending_removal_target(
    request: &ConversationUpdateRequest,
    evidence: Option<&ViolationEvidence>,
    approved: bool,
    is_emergency: bool,
    transforms_to_removal: bool,
) -> Option<Vec<u8>> {
    if !approved {
        return None;
    }
    if transforms_to_removal {
        return evidence.map(|ev| ev.target_member_id.clone());
    }
    if is_emergency {
        return None;
    }
    match change_of(request) {
        Some((ActionKind::Remove, id)) => Some(id.to_vec()),
        _ => None,
    }
}
