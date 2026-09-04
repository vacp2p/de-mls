//! The pure half of outcome handling: given a resolved proposal's decision
//! and the request it carried, [`apply_outcome`] reshapes [`EngineQueues`]
//! and names the follow-up the acting half in the parent module still owes.

use tracing::info;

use crate::{
    engine::queues::EngineQueues,
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
    /// The election failed. Nothing automatic follows.
    ElectionRejected,
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

/// Classify a resolved proposal and apply its queue effects.
///
/// What happens follows from `approved` and the request kind: accepted
/// membership changes move to the approved queue for the next commit; an
/// accepted below-threshold emergency becomes a `RemoveMember` with an urgent
/// commit; an accepted Deadlock skips the current epoch steward; an accepted
/// election is handed back to install; rejected proposals are dropped.
///
/// One rule that isn't obvious from the branches: **removal dedup** — the same
/// member can be removed by several paths (self-leave, ban, below-threshold)
/// under different proposal ids. Only the first reaches the approved queue,
/// because the group rejects a duplicate at commit time. Invites dedup the
/// same way, keyed by the joiner id the proposer stamped on the request.
pub(crate) fn apply_outcome(
    queues: &mut EngineQueues,
    proposal_id: u32,
    approved: bool,
    request: &ConversationUpdateRequest,
) -> ApplyOutcome {
    // The outcome resolved this proposal, so it leaves the in-flight queue.
    // On YES the approved queue becomes its record (below).
    queues.remove_voting_proposal(proposal_id);

    if let Some(election) = extract_election_proposal(request).cloned() {
        return apply_election_result(approved, election);
    }

    // ── Emergency and regular proposals ──

    let evidence = extract_emergency_evidence(request).cloned();
    let is_emergency = evidence.is_some();

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

    // Two approvals from independent paths (self-leave + ban, ECP + ban, …)
    // can each carry `RemoveMember(target)` under different proposal ids. The
    // group rejects a duplicate removal at commit time, so keep the first
    // entry and drop the second.
    if let Some(target) = &removal_target
        && queues.has_approved_removal(target)
    {
        info!(
            proposal_id,
            target = ?target,
            "removal proposal deduped — target already queued for removal"
        );
        return ApplyOutcome::NoAction;
    }

    // Two approved proposals can admit one member (an invitation racing a
    // sponsored join) with key packages the group won't collapse into one.
    // Drop the duplicate, like removals above, or the whole batch is rejected.
    if approved
        && let Some(conversation_update_request::Payload::MemberInvite(invite)) =
            request.payload.as_ref()
        && !invite.member_id.is_empty()
        && queues.has_approved_invite(&invite.member_id)
    {
        info!(
            proposal_id,
            target = ?invite.member_id,
            "invite proposal deduped — target already queued for admission"
        );
        return ApplyOutcome::NoAction;
    }

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
                "emergency criteria proposal rejected"
            );
        }
    }

    // Below-threshold ECP: it becomes a `RemoveMember`, and the next commit is
    // restricted to that target so the removal doesn't drag along unrelated
    // approved work. `transforms_to_removal` implies `approved` and `evidence`
    // is `Some`; `filter` binds it without an unwrap.
    if let Some(ev) = evidence.as_ref().filter(|_| transforms_to_removal) {
        queues.insert_approved_proposal(proposal_id, removal_request_for(ev));
        let target = ev.target_member_id.clone();
        queues.set_urgent_commit_target(target.clone());
        return ApplyOutcome::UrgentRemoval { target };
    }

    // Regular proposals stage for the next commit; other emergencies produce
    // no membership change, so there is nothing to stage.
    if approved && !is_emergency {
        queues.insert_approved_proposal(proposal_id, request.clone());
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
/// list back for validation and install; NO just reports the rejection.
fn apply_election_result(approved: bool, election: StewardElectionProposal) -> ApplyOutcome {
    if approved {
        info!(
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            "steward election proposal accepted"
        );
        ApplyOutcome::ElectionAccepted(election)
    } else {
        info!("steward election proposal rejected");
        ApplyOutcome::ElectionRejected
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
    match request.payload.as_ref() {
        Some(conversation_update_request::Payload::RemoveMember(r)) => Some(r.member_id.clone()),
        _ => None,
    }
}
