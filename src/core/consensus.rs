//! Pure consensus result application.
//!
//! Updates a [`ConversationQueues`] in response to a consensus outcome.

use tracing::info;

use crate::{
    core::{ConversationQueues, CoreError, target_member_id_of},
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, ViolationEvidence, ViolationType,
        conversation_update_request,
    },
};

/// Outcome of applying a consensus result to a conversation. Each variant
/// encodes exactly the follow-up the app caller must perform
#[derive(Debug, Clone)]
pub enum ConsensusApplyResult {
    /// Nothing for the caller to do. The contract is "no follow-up", not
    /// "no change" — some of these still mutated the approved queue.
    NoAction,
    /// Election accepted. Caller validates the proposed list against the
    /// candidate pool, installs it, and exits Reelection.
    ElectionAccepted(StewardElectionProposal),
    /// Election rejected. Caller runs the reelection-retry / escalation path.
    ElectionRejected,
    /// A membership proposal (Add/Remove) was rejected. Caller drops the
    /// buffered pending-update for `target`.
    RejectedMembership { target: Vec<u8> },
    /// Layer-3 `Deadlock` ECP accepted. Caller switches the conversation's
    /// [`crate::core::OperatingMode`] to `Recovery` (any-member commit) and
    /// bypasses the inactivity timer. Cleared on the next accepted election.
    RecoveryModeOpened,
    /// `SCORE_BELOW_THRESHOLD` ECP accepted. The urgent-commit target is
    /// already set on the conversation; caller bypasses the inactivity
    /// timer so the urgent commit fires now and refreshes the steward
    /// list if `target` was on it.
    UrgentRemoval { target: Vec<u8> },
    /// Regular `RemoveMember` accepted and queued for the next commit.
    /// Caller refreshes the steward list if `target` was on it.
    QueuedRemoval { target: Vec<u8> },
}

/// Apply a consensus result to the conversation's proposal queues.
///
/// Routes by proposal kind:
/// - **Election (accepted)** — returns [`ConsensusApplyResult::ElectionAccepted`].
/// - **`ScoreBelowThreshold` ECP (accepted)** — queues `RemoveMember(target)`
///   in the approved queue, sets the urgent-commit target, and returns
///   [`ConsensusApplyResult::UrgentRemoval`].
/// - **`Deadlock` ECP (accepted)** — returns
///   [`ConsensusApplyResult::RecoveryModeOpened`]. No approved-queue entry
///   (no MLS op to commit).
/// - **Other emergency (accepted)** — transient: briefly marked approved
///   then removed. Returns [`ConsensusApplyResult::NoAction`].
/// - **Regular `RemoveMember` (accepted)** — moved to the approved queue;
///   returns [`ConsensusApplyResult::QueuedRemoval`]. Duplicate-target
///   removals are deduped at insertion and return `NoAction`.
/// - **Other regular proposal (accepted)** — moved to the approved queue;
///   returns [`ConsensusApplyResult::NoAction`].
/// - **Election rejected** — returns [`ConsensusApplyResult::ElectionRejected`].
/// - **Membership rejected** — returns
///   [`ConsensusApplyResult::RejectedMembership`].
/// - **Other rejected** — dropped from the voting queue if we owned it;
///   returns [`ConsensusApplyResult::NoAction`].
///
/// The caller decodes the payload once and passes the request in; this
/// function never re-parses bytes.
pub fn apply_consensus_result(
    conversation: &mut ConversationQueues,
    proposal_id: u32,
    approved: bool,
    request: &ConversationUpdateRequest,
) -> Result<ConsensusApplyResult, CoreError> {
    let is_owner = conversation.is_owner_of_proposal(proposal_id);
    let evidence = extract_emergency_evidence(request).cloned();
    let is_emergency = evidence.is_some();

    if let Some(election) = extract_election_proposal(request).cloned() {
        return Ok(apply_election_outcome(
            conversation,
            proposal_id,
            approved,
            election,
            is_owner,
        ));
    }

    // ── Emergency and regular proposals ──

    // Should the approved ECP transform into a RemoveMember?
    let transforms_to_removal =
        approved && is_emergency && evidence.as_ref().is_some_and(is_score_below_threshold);

    // Target for `approved_proposals` dedup and downstream steward-list refresh.
    let removal_target = pending_removal_target(
        request,
        evidence.as_ref(),
        approved,
        is_emergency,
        transforms_to_removal,
    );

    // Two approvals from independent paths (self-leave + ban, ECP + ban, …)
    // can each carry `RemoveMember(target)` under different proposal ids.
    // MLS rejects a duplicate removal at commit time, so keep the first
    // entry and drop the second.
    if let Some(target) = &removal_target
        && conversation.has_approved_removal(target)
    {
        if is_owner {
            conversation.remove_voting_proposal(proposal_id);
        }
        info!(
            proposal_id,
            target = ?target,
            "removal proposal deduped — target already queued for removal"
        );
        return Ok(ConsensusApplyResult::NoAction);
    }

    // `transforms_to_removal` implies `evidence` is `Some`; `filter` lets the
    // branches below bind it without an unwrap.
    let sbt_evidence = evidence.as_ref().filter(|_| transforms_to_removal);

    if approved {
        if is_owner {
            conversation.mark_proposal_as_approved(proposal_id);
            if let Some(ev) = sbt_evidence {
                // Replace ECP with RemoveMember in approved queue (reuse proposal_id).
                let removal = removal_request_for(ev);
                conversation.remove_approved_proposal(proposal_id);
                conversation.insert_approved_proposal(proposal_id, removal);
            } else if is_emergency {
                // Other emergencies don't produce MLS operations.
                conversation.remove_approved_proposal(proposal_id);
            }
        } else if let Some(ev) = sbt_evidence {
            // Non-owner: insert RemoveMember directly (the ECP was never stored).
            let removal = removal_request_for(ev);
            conversation.insert_approved_proposal(proposal_id, removal);
        } else if !is_emergency {
            // Regular proposal: add to approved queue for the next commit.
            conversation.insert_approved_proposal(proposal_id, request.clone());
        }
    } else if is_owner {
        conversation.remove_voting_proposal(proposal_id);
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

    if let Some(ev) = sbt_evidence {
        // Fast removal: restrict the next commit to this target so it
        // doesn't drag along unrelated approved work.
        let target = ev.target_member_id.clone();
        conversation.set_urgent_commit_target(target.clone());
        return Ok(ConsensusApplyResult::UrgentRemoval { target });
    }
    if evidence.as_ref().is_some_and(is_deadlock) && approved {
        // Layer 3: any member can produce the next commit.
        return Ok(ConsensusApplyResult::RecoveryModeOpened);
    }
    if let Some(target) = removal_target {
        return Ok(ConsensusApplyResult::QueuedRemoval { target });
    }
    // Rejected membership: caller drops the buffered pending-update.
    if !approved && let Some(target) = target_member_id_of(request) {
        return Ok(ConsensusApplyResult::RejectedMembership {
            target: target.to_vec(),
        });
    }
    Ok(ConsensusApplyResult::NoAction)
}

/// Election outcome — no MLS operation. YES hands the proposed list
/// back to the app for validation and install; NO drops the owner's
/// voting-queue entry.
fn apply_election_outcome(
    conversation: &mut ConversationQueues,
    proposal_id: u32,
    approved: bool,
    election: StewardElectionProposal,
    is_owner: bool,
) -> ConsensusApplyResult {
    if approved {
        if is_owner {
            conversation.mark_proposal_as_approved(proposal_id);
            conversation.remove_approved_proposal(proposal_id);
        }
        info!(
            proposal_id,
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            "steward election proposal accepted"
        );
        ConsensusApplyResult::ElectionAccepted(election)
    } else {
        if is_owner {
            conversation.remove_voting_proposal(proposal_id);
        }
        info!(proposal_id, "steward election proposal rejected");
        ConsensusApplyResult::ElectionRejected
    }
}

/// Extract emergency evidence from a `ConversationUpdateRequest`, if present.
fn extract_emergency_evidence(req: &ConversationUpdateRequest) -> Option<&ViolationEvidence> {
    match &req.payload {
        Some(conversation_update_request::Payload::EmergencyCriteria(ec)) => ec.evidence.as_ref(),
        _ => None,
    }
}

/// Extract a steward election proposal from a `ConversationUpdateRequest`, if present.
fn extract_election_proposal(req: &ConversationUpdateRequest) -> Option<&StewardElectionProposal> {
    match &req.payload {
        Some(conversation_update_request::Payload::StewardElection(se)) => Some(se),
        _ => None,
    }
}

/// Check whether evidence is a `SCORE_BELOW_THRESHOLD` violation.
fn is_score_below_threshold(evidence: &ViolationEvidence) -> bool {
    ViolationType::try_from(evidence.violation_type) == Ok(ViolationType::ScoreBelowThreshold)
}

/// Check whether evidence is the `DEADLOCK` (Layer 3 anti-deadlock) signal.
fn is_deadlock(evidence: &ViolationEvidence) -> bool {
    ViolationType::try_from(evidence.violation_type) == Ok(ViolationType::Deadlock)
}

/// Build a `RemoveMember` `ConversationUpdateRequest` for the target in score-below-threshold evidence.
fn removal_request_for(evidence: &ViolationEvidence) -> ConversationUpdateRequest {
    ConversationUpdateRequest::remove_member(evidence.target_member_id.clone())
}

/// Identity this approval would queue for removal in `approved_proposals`,
/// if any. Covers a direct `RemoveMember` request and a score-below-threshold
/// ECP that transforms into one. Returns `None` for elections, non-removal
/// emergencies, non-removal regular proposals, and rejections.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::steward_list::{StewardList, StewardListConfig};
    use crate::protos::de_mls::messages::v1::{ConversationUpdateRequest, StewardElectionProposal};

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    fn election_request(stewards: Vec<Vec<u8>>, epoch: u64) -> ConversationUpdateRequest {
        ConversationUpdateRequest::steward_election(StewardElectionProposal {
            proposed_stewards: stewards,
            election_epoch: epoch,
            retry_round: 0,
        })
    }

    /// YES on an election the local node owns returns the accepted proposal
    /// and doesn't leave the proposal in the approved queue.
    #[test]
    fn election_yes_owner_returns_outcome_and_clears_queue() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mut conversation = ConversationQueues::new("test-conversation");
        let mems = members(&[1, 2, 3, 4, 5]);
        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-conversation", &mems, sn, config, 0).unwrap();
        let request = election_request(list.members().to_vec(), 10);

        let proposal_id = 42;
        conversation.insert_voting_proposal(proposal_id, request.clone());

        let result =
            apply_consensus_result(&mut conversation, proposal_id, true, &request).unwrap();

        let ConsensusApplyResult::ElectionAccepted(outcome) = result else {
            panic!("expected ElectionAccepted, got {result:?}");
        };
        assert_eq!(outcome.election_epoch, 10);
        assert_eq!(outcome.proposed_stewards.len(), 5);
        assert_eq!(conversation.approved_proposals_count(), 0);
    }

    /// NO on an election returns `ElectionRejected` and leaves the approved
    /// queue empty.
    #[test]
    fn election_no_returns_election_rejected() {
        let mut conversation = ConversationQueues::new("test-conversation");
        let request = election_request(vec![member(1), member(2)], 10);

        let proposal_id = 43;
        conversation.insert_voting_proposal(proposal_id, request.clone());

        let result =
            apply_consensus_result(&mut conversation, proposal_id, false, &request).unwrap();

        assert!(matches!(result, ConsensusApplyResult::ElectionRejected));
        assert_eq!(conversation.approved_proposals_count(), 0);
    }

    /// YES on an election the local node *doesn't* own still returns the outcome
    /// (non-owner path), and doesn't touch any proposal queues.
    #[test]
    fn election_yes_nonowner_returns_outcome_without_queue_side_effects() {
        let mut conversation = ConversationQueues::new("test-conversation");
        let request = election_request(vec![member(1), member(2), member(3)], 5);

        let proposal_id = 44;
        let result =
            apply_consensus_result(&mut conversation, proposal_id, true, &request).unwrap();

        let ConsensusApplyResult::ElectionAccepted(outcome) = result else {
            panic!("expected ElectionAccepted, got {result:?}");
        };
        assert_eq!(outcome.election_epoch, 5);
        assert_eq!(outcome.proposed_stewards.len(), 3);
        assert_eq!(conversation.approved_proposals_count(), 0);
    }

    fn remove_request(target: Vec<u8>) -> ConversationUpdateRequest {
        ConversationUpdateRequest::remove_member(target)
    }

    /// A second `RemoveMember(target)` arriving via consensus is dropped
    /// when an entry for the same target is already in `approved_proposals`.
    #[test]
    fn removal_deduped_when_target_already_pending() {
        let mut conversation = ConversationQueues::new("test-conversation");
        let target = member(7);

        // First removal — non-owner path inserts straight into approved.
        let first_id = 10;
        let request = remove_request(target.clone());
        let first_result =
            apply_consensus_result(&mut conversation, first_id, true, &request).unwrap();
        assert!(matches!(
            first_result,
            ConsensusApplyResult::QueuedRemoval { .. }
        ));
        assert_eq!(conversation.approved_proposals_count(), 1);

        // Second removal for the same target arrives under a different id.
        let second_id = 11;
        let request = remove_request(target.clone());
        let result = apply_consensus_result(&mut conversation, second_id, true, &request).unwrap();

        assert!(matches!(result, ConsensusApplyResult::NoAction));
        assert_eq!(
            conversation.approved_proposals_count(),
            1,
            "duplicate removal must not stack a second entry"
        );
        assert!(conversation.approved_proposals().contains_key(&first_id));
        assert!(!conversation.approved_proposals().contains_key(&second_id));
    }

    /// Owner-side dedup: the duplicate clears its voting-queue entry so the
    /// queue does not retain an outcome we deliberately discarded.
    #[test]
    fn removal_dedup_clears_owner_voting_entry() {
        let mut conversation = ConversationQueues::new("test-conversation");
        let target = member(7);

        // Pre-existing approved removal from an unrelated path.
        let pending_id = 20;
        let pending = remove_request(target.clone());
        conversation.insert_approved_proposal(pending_id, pending);

        // This user submits their own removal and it passes consensus.
        let owner_id = 21;
        let owner_request = remove_request(target.clone());
        conversation.insert_voting_proposal(owner_id, owner_request.clone());

        let result =
            apply_consensus_result(&mut conversation, owner_id, true, &owner_request).unwrap();

        assert!(matches!(result, ConsensusApplyResult::NoAction));
        assert_eq!(conversation.approved_proposals_count(), 1);
        assert!(conversation.approved_proposals().contains_key(&pending_id));
        assert!(
            !conversation.is_owner_of_proposal(owner_id),
            "duplicate must be cleared from voting queue"
        );
    }

    fn score_below_threshold_request(
        target: Vec<u8>,
        creator: Vec<u8>,
    ) -> ConversationUpdateRequest {
        ViolationEvidence::score_below_threshold(target, 0, -10)
            .with_creator(creator)
            .into_update_request()
            .unwrap()
    }

    #[test]
    fn ecp_score_below_threshold_yes_returns_urgent_removal() {
        let mut conversation = ConversationQueues::new("urgent-yes");
        let target = member(7);

        let request = score_below_threshold_request(target.clone(), member(1));

        let result = apply_consensus_result(&mut conversation, 100, true, &request).unwrap();

        let ConsensusApplyResult::UrgentRemoval { target: out_target } = result else {
            panic!("expected UrgentRemoval, got {result:?}");
        };
        assert_eq!(out_target, target);
        assert_eq!(
            conversation.urgent_commit_target(),
            Some(target.as_slice()),
            "urgent-commit target must be set on the conversation"
        );
        assert_eq!(
            conversation.approved_proposals_count(),
            1,
            "RemoveMember queued"
        );
    }

    #[test]
    fn ecp_score_below_threshold_no_does_not_mark_urgent() {
        let mut conversation = ConversationQueues::new("urgent-no");
        let request = score_below_threshold_request(member(7), member(1));

        let result = apply_consensus_result(&mut conversation, 101, false, &request).unwrap();

        assert!(matches!(result, ConsensusApplyResult::NoAction));
        assert!(conversation.urgent_commit_target().is_none());
        assert_eq!(conversation.approved_proposals_count(), 0);
    }

    fn deadlock_request(creator: Vec<u8>) -> ConversationUpdateRequest {
        ViolationEvidence::deadlock(0)
            .with_creator(creator)
            .into_update_request()
            .unwrap()
    }

    #[test]
    fn ecp_deadlock_yes_returns_recovery_mode_opened() {
        let mut conversation = ConversationQueues::new("deadlock-yes");

        let request = deadlock_request(member(1));
        let result = apply_consensus_result(&mut conversation, 200, true, &request).unwrap();

        assert!(matches!(result, ConsensusApplyResult::RecoveryModeOpened));
        assert_eq!(
            conversation.approved_proposals_count(),
            0,
            "Deadlock has no specific target — no RemoveMember queued"
        );
        assert!(conversation.urgent_commit_target().is_none());
    }

    #[test]
    fn ecp_deadlock_no_returns_no_action() {
        let mut conversation = ConversationQueues::new("deadlock-no");

        let request = deadlock_request(member(1));
        let result = apply_consensus_result(&mut conversation, 201, false, &request).unwrap();

        assert!(matches!(result, ConsensusApplyResult::NoAction));
    }

    /// A regular (non-emergency) `RemoveMember` reached via consensus YES
    /// enqueues into `approved_proposals` and produces no score ops.
    #[test]
    fn regular_remove_member_enqueues_without_score_ops() {
        let mut conversation = ConversationQueues::new("regular-yes");
        let target = member(7);

        let request = remove_request(target.clone());

        let proposal_id = 70;
        conversation.insert_voting_proposal(proposal_id, request.clone());

        apply_consensus_result(&mut conversation, proposal_id, true, &request).unwrap();

        assert!(crate::core::emergency_score_ops(&request, true).is_empty());
        assert_eq!(conversation.approved_proposals_count(), 1);
    }

    /// A non-score emergency (e.g. `BrokenCommit`) approved by consensus
    /// is consumed without queuing a `RemoveMember` — only score-below-
    /// threshold ECPs transform into a removal.
    #[test]
    fn regular_emergency_yes_does_not_queue_remove_member() {
        let mut conversation = ConversationQueues::new("no-transform");
        let creator = member(1);
        let target = member(7);

        let request = ViolationEvidence::broken_commit(target, 0, Vec::<u8>::new())
            .with_creator(creator)
            .into_update_request()
            .unwrap();

        let proposal_id = 300;
        conversation.insert_voting_proposal(proposal_id, request.clone());

        apply_consensus_result(&mut conversation, proposal_id, true, &request).unwrap();

        assert_eq!(
            conversation.approved_proposals_count(),
            0,
            "regular emergencies are consumed, not transformed to RemoveMember"
        );
    }
}
