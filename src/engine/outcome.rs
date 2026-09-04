//! What a resolved consensus proposal does: the queue effects it has, and
//! the follow-up the engine still owes once those are applied.
//!
//! [`apply_outcome`] is the pure half — given the decision and the request it
//! carried, it reshapes [`EngineQueues`] and names the follow-up.
//! [`Engine::handle_consensus_outcome`] is the acting half: it reports the
//! decision, runs [`apply_outcome`], and then installs an election, skips a
//! silent epoch steward, freezes for an urgent removal, or applies emergency
//! scores.

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use prost::Message;
use tracing::{debug, info, warn};

use crate::{
    ConversationError, ScoreOp,
    engine::{
        handle::Engine,
        queues::EngineQueues,
        store::EngineStore,
        types::{Event, MemberId},
        util::target_member_id_of,
    },
    peer_scoring::emergency_score_ops,
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, ViolationEvidence, ViolationType,
        conversation_update_request,
    },
};

/// What [`apply_outcome`] decided. The queue changes are already done; each
/// variant names the follow-up still owed — install an election, skip a
/// silent epoch steward, commit a removal, and so on.
#[derive(Debug, Clone)]
enum ApplyOutcome {
    /// Nothing left to do. "No follow-up", not "no change" — some of these
    /// paths still adjusted the approved queue.
    NoAction,
    /// The election passed. Validate the proposed list and install it.
    ElectionAccepted(StewardElectionProposal),
    /// The election failed. Nothing automatic follows.
    ElectionRejected,
    /// An Add/Remove was voted down. Drop the buffered pending update for
    /// `target`.
    RejectedMembership { target: Vec<u8> },
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
fn apply_outcome(
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
    // Rejected membership: the caller drops the buffered pending update.
    if !approved && let Some(target) = target_member_id_of(request) {
        return ApplyOutcome::RejectedMembership { target };
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

impl<St: EngineStore> Engine<St> {
    /// Pop every resolved outcome and apply it. Called at the end of every
    /// driving call, so an outcome reached mid-call rides out on that call
    /// instead of waiting for the next wakeup.
    pub(crate) fn drain_consensus_outcomes(&mut self) {
        while let Some((_scope, event)) = self.consensus_rx.try_recv() {
            if let Err(e) = self.handle_consensus_outcome(event) {
                self.report_failure("handle_consensus_outcome", &e);
            }
        }
    }

    /// Handle one resolved decision: report it, update the proposal queues
    /// via [`apply_outcome`], then run the follow-up it asks for.
    pub(crate) fn handle_consensus_outcome(
        &mut self,
        event: ConsensusEvent,
    ) -> Result<(), ConversationError> {
        let (proposal_id, approved, timestamp) = match &event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
                timestamp,
            } => (*proposal_id, *result, *timestamp),
            ConsensusEvent::ConsensusFailed {
                proposal_id,
                timestamp,
            } => (*proposal_id, false, *timestamp),
        };

        // Any outcome moots both pending deadlines for this proposal.
        self.cancel_auto_vote(proposal_id);
        self.unregister_consensus_timeout(proposal_id);
        if self.queues.is_consensus_outcome_applied(proposal_id) {
            debug!(
                conversation = %self.conversation_id,
                proposal_id,
                "duplicate consensus outcome dropped"
            );
            return Ok(());
        }

        // Reported before any effects, so the router sees the decision in the
        // same output as the state changes it triggers.
        self.emit(Event::ConsensusReached {
            proposal_id,
            approved,
            timestamp,
        });
        let scope = self.conversation_id.clone();
        let proposal = self.consensus.storage().get_proposal(&scope, proposal_id)?;
        let request = ConversationUpdateRequest::decode(proposal.payload.as_slice())?;

        // Nothing arms the inactivity timer here — the next tick's inactivity
        // check self-starts it once it sees approved work.
        info!(
            conversation = %self.conversation_id,
            proposal_id, approved, "consensus reached"
        );
        self.queues.mark_consensus_outcome_applied(proposal_id);
        let outcome = apply_outcome(&mut self.queues, proposal_id, approved, &request);

        // A peer steward can reach consensus and broadcast its commit
        // candidate before our own outcome lands. Now that the approved queue
        // is populated, replay any stashed candidate so the commit round
        // starts with it instead of empty (which would otherwise close with
        // no commit and cost a full round).
        self.replay_early_candidates()?;

        match outcome {
            ApplyOutcome::NoAction => {}
            ApplyOutcome::ElectionAccepted(election) => {
                self.handle_election_accepted(election)?;
            }
            // No retry ladder: the next request_recovery / propose_election
            // call is the application's move.
            ApplyOutcome::ElectionRejected => {}
            ApplyOutcome::DeadlockAccepted => self.skip_silent_epoch_steward(),
            ApplyOutcome::UrgentRemoval { target } => {
                // A steward-gated removal: enter freezing and mint it now.
                if let Some(event) = self.start_freezing() {
                    self.on_freeze_entered(event)?;
                }
                self.refresh_stewards_after_removal(&target)?;
            }
            ApplyOutcome::QueuedRemoval { target } => {
                self.refresh_stewards_after_removal(&target)?;
            }
            ApplyOutcome::RejectedMembership { target } => {
                self.queues.remove_pending_update(&target);
            }
        }

        // Score changes are only triggered for emergency proposals.
        let score_ops = emergency_score_ops(&request, approved);
        if !score_ops.is_empty() {
            self.handle_emergency_scored(proposal_id, &score_ops)?;
        }

        // Free the resolved session so the store keeps only live ones; late
        // votes and timeouts for this proposal fall back to the resolved cache
        // recorded above.
        self.consensus
            .storage()
            .remove_session(&scope, proposal_id)?;
        // store: consensus sessions (WP3g)

        Ok(())
    }

    /// A `Deadlock` proposal passed: skip the eligibility-filtered epoch
    /// steward for this epoch, or report that none is left to skip.
    fn skip_silent_epoch_steward(&mut self) {
        let expected = {
            let eligible = self.queues.steward_eligibility(&self.members);
            self.steward_list
                .epoch_steward(self.epoch, &eligible)
                .map(<[u8]>::to_vec)
        };
        match expected {
            Some(es) => {
                self.queues.skip_steward(es.clone());
                self.emit(Event::StewardSkipped {
                    epoch: self.epoch,
                    steward: MemberId::from(es.as_slice()),
                });
            }
            None => {
                self.emit(Event::CommitMissing {
                    epoch: self.epoch,
                    steward: None,
                });
            }
        }
    }

    /// A removal that hits a current steward triggers a fresh election so the
    /// next epoch still has a live epoch and backup steward. Election
    /// rejection here is deferred, not fatal — the list heals on a later
    /// reconcile.
    fn refresh_stewards_after_removal(&mut self, target: &[u8]) -> Result<(), ConversationError> {
        if !self.steward_list.is_steward(target) {
            return Ok(());
        }
        if let Err(e) = self.initiate_steward_election() {
            warn!(
                conversation = %self.conversation_id,
                error = %e,
                "post-removal steward-list refresh failed"
            );
            self.report_failure("post_removal_election", &e);
        }
        Ok(())
    }

    /// Validate and install an accepted steward list, then drain buffered
    /// updates so the fresh epoch steward proposes them.
    fn handle_election_accepted(
        &mut self,
        election: StewardElectionProposal,
    ) -> Result<(), ConversationError> {
        if !self.validate_election_list(&election)? {
            info!(
                conversation = %self.conversation_id,
                "steward election rejected: invalid list"
            );
            return Ok(());
        }

        self.steward_list.install_list(
            election.election_epoch,
            &election.proposed_stewards,
            election.proposed_stewards.len(),
            election.retry_round,
        )?;
        info!(
            conversation = %self.conversation_id,
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            retry_round = election.retry_round,
            "steward election applied"
        );
        // store: steward list (WP3g)

        // Broadcast the elected list so a member that missed the vote learns
        // it and authorizes the next steward's commit. Drain buffered updates
        // regardless, then surface a share failure.
        let shared = if self.is_epoch_steward() {
            self.share_conversation_sync()
        } else {
            Ok(())
        };
        self.process_buffered_updates()?;
        shared
    }

    /// A finished emergency: apply its score ops and clear the partial
    /// freeze.
    fn handle_emergency_scored(
        &mut self,
        proposal_id: u32,
        score_ops: &[ScoreOp],
    ) -> Result<(), ConversationError> {
        self.apply_score_ops(score_ops);
        self.queues.remove_emergency(proposal_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::steward_list::{StewardList, StewardListConfig};

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

    fn remove_request(target: Vec<u8>) -> ConversationUpdateRequest {
        ConversationUpdateRequest::remove_member(target)
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

    fn deadlock_request(creator: Vec<u8>) -> ConversationUpdateRequest {
        ViolationEvidence::deadlock(0)
            .with_creator(creator)
            .into_update_request()
            .unwrap()
    }

    /// YES on an election hands the accepted list back and leaves nothing in
    /// the approved queue.
    #[test]
    fn election_yes_returns_outcome_and_clears_voting() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mut queues = EngineQueues::new(10);
        let mems = members(&[1, 2, 3, 4, 5]);
        let sn = mems.len().min(config.sn_max);
        let list = StewardList::generate(10, b"test-conversation", &mems, sn, config, 0).unwrap();
        let request = election_request(list.members().to_vec(), 10);

        let proposal_id = 42;
        queues.track_voting_proposal(proposal_id, &request);

        let result = apply_outcome(&mut queues, proposal_id, true, &request);

        let ApplyOutcome::ElectionAccepted(outcome) = result else {
            panic!("expected ElectionAccepted, got {result:?}");
        };
        assert_eq!(outcome.election_epoch, 10);
        assert_eq!(outcome.proposed_stewards.len(), 5);
        // An election never stages into approved, and its voting entry clears.
        assert_eq!(queues.approved_proposals_count(), 0);
        assert!(!queues.is_voting(proposal_id));
    }

    /// NO on an election reports the rejection and stages nothing.
    #[test]
    fn election_no_returns_election_rejected() {
        let mut queues = EngineQueues::new(10);
        let request = election_request(vec![member(1), member(2)], 10);

        let proposal_id = 43;
        queues.track_voting_proposal(proposal_id, &request);

        let result = apply_outcome(&mut queues, proposal_id, false, &request);

        assert!(matches!(result, ApplyOutcome::ElectionRejected));
        assert_eq!(queues.approved_proposals_count(), 0);
    }

    /// A second `RemoveMember(target)` is dropped when an entry for the same
    /// target is already approved.
    #[test]
    fn removal_deduped_when_target_already_pending() {
        let mut queues = EngineQueues::new(10);
        let target = member(7);

        let first_id = 10;
        let request = remove_request(target.clone());
        let first_result = apply_outcome(&mut queues, first_id, true, &request);
        assert!(matches!(first_result, ApplyOutcome::QueuedRemoval { .. }));
        assert_eq!(queues.approved_proposals_count(), 1);

        let second_id = 11;
        let request = remove_request(target.clone());
        let result = apply_outcome(&mut queues, second_id, true, &request);

        assert!(matches!(result, ApplyOutcome::NoAction));
        assert_eq!(
            queues.approved_proposals_count(),
            1,
            "duplicate removal must not stack a second entry"
        );
        assert!(queues.approved_proposals().contains_key(&first_id));
        assert!(!queues.approved_proposals().contains_key(&second_id));
    }

    /// Removal dedup clears the duplicate's voting entry so the queue does
    /// not retain an outcome we deliberately discarded.
    #[test]
    fn removal_dedup_clears_voting_entry() {
        let mut queues = EngineQueues::new(10);
        let target = member(7);

        let pending_id = 20;
        queues.insert_approved_proposal(pending_id, remove_request(target.clone()));

        let dup_id = 21;
        let dup_request = remove_request(target.clone());
        queues.track_voting_proposal(dup_id, &dup_request);

        let result = apply_outcome(&mut queues, dup_id, true, &dup_request);

        assert!(matches!(result, ApplyOutcome::NoAction));
        assert_eq!(queues.approved_proposals_count(), 1);
        assert!(queues.approved_proposals().contains_key(&pending_id));
        assert!(
            !queues.is_voting(dup_id),
            "duplicate must be cleared from voting queue"
        );
    }

    #[test]
    fn ecp_score_below_threshold_yes_returns_urgent_removal() {
        let mut queues = EngineQueues::new(10);
        let target = member(7);

        let request = score_below_threshold_request(target.clone(), member(1));

        let result = apply_outcome(&mut queues, 100, true, &request);

        let ApplyOutcome::UrgentRemoval { target: out_target } = result else {
            panic!("expected UrgentRemoval, got {result:?}");
        };
        assert_eq!(out_target, target);
        assert_eq!(
            queues.urgent_commit_target(),
            Some(target.as_slice()),
            "urgent-commit target must be set"
        );
        assert_eq!(queues.approved_proposals_count(), 1, "RemoveMember queued");
    }

    #[test]
    fn ecp_deadlock_yes_returns_deadlock_accepted() {
        let mut queues = EngineQueues::new(10);

        let request = deadlock_request(member(1));
        let result = apply_outcome(&mut queues, 200, true, &request);

        assert!(matches!(result, ApplyOutcome::DeadlockAccepted));
        assert_eq!(
            queues.approved_proposals_count(),
            0,
            "Deadlock has no specific target — no RemoveMember queued"
        );
        assert!(queues.urgent_commit_target().is_none());
    }

    /// A rejected emergency does nothing at all, whatever it proposed. Every
    /// branch that acts is gated on `approved`, so this holds for the whole
    /// family — and it is what keeps a below-threshold removal a group
    /// decision: any member can raise one, but a NO leaves no trace.
    #[test]
    fn rejected_emergency_leaves_no_trace() {
        for (name, request) in [
            ("deadlock-no", deadlock_request(member(1))),
            (
                "score-below-threshold-no",
                score_below_threshold_request(member(7), member(1)),
            ),
        ] {
            let mut queues = EngineQueues::new(10);
            let result = apply_outcome(&mut queues, 201, false, &request);

            assert!(matches!(result, ApplyOutcome::NoAction), "{name}");
            assert!(queues.urgent_commit_target().is_none(), "{name}");
            assert_eq!(queues.approved_proposals_count(), 0, "{name}");
        }
    }

    /// A regular `RemoveMember` reached by YES enqueues and produces no score
    /// ops.
    #[test]
    fn regular_remove_member_enqueues_without_score_ops() {
        let mut queues = EngineQueues::new(10);
        let request = remove_request(member(7));

        let proposal_id = 70;
        queues.track_voting_proposal(proposal_id, &request);

        apply_outcome(&mut queues, proposal_id, true, &request);

        assert!(emergency_score_ops(&request, true).is_empty());
        assert_eq!(queues.approved_proposals_count(), 1);
    }

    /// A non-score emergency approved by consensus is consumed without
    /// queuing a `RemoveMember` — only score-below-threshold ECPs transform
    /// into one.
    #[test]
    fn regular_emergency_yes_does_not_queue_remove_member() {
        let mut queues = EngineQueues::new(10);

        let request = ViolationEvidence::broken_commit(member(7), 0, Vec::<u8>::new())
            .with_creator(member(1))
            .into_update_request()
            .unwrap();

        let proposal_id = 300;
        queues.track_voting_proposal(proposal_id, &request);

        apply_outcome(&mut queues, proposal_id, true, &request);

        assert_eq!(
            queues.approved_proposals_count(),
            0,
            "regular emergencies are consumed, not transformed to RemoveMember"
        );
    }

    /// An invite for a joiner already queued for admission is dropped: the
    /// group rejects a batch that admits one member twice.
    #[test]
    fn invite_deduped_when_joiner_already_queued() {
        use crate::protos::de_mls::messages::v1::MemberInvite;

        let joiner = member(9);
        let invite = |kp: &[u8]| ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::MemberInvite(
                MemberInvite {
                    key_package_bytes: kp.to_vec(),
                    member_id: joiner.clone(),
                },
            )),
        };

        let mut queues = EngineQueues::new(10);
        apply_outcome(&mut queues, 1, true, &invite(b"kp-a"));
        assert_eq!(queues.approved_proposals_count(), 1);

        let result = apply_outcome(&mut queues, 2, true, &invite(b"kp-b"));
        assert!(matches!(result, ApplyOutcome::NoAction));
        assert_eq!(queues.approved_proposals_count(), 1);
    }
}

#[cfg(test)]
mod deadlock_skip_tests {
    use super::*;
    use crate::engine::{steward::test_support::founder, types::Timestamp};

    /// A `Deadlock` YES skips the epoch steward for this epoch: the skip is
    /// recorded, `StewardSkipped` names it, and the rotation then hands the
    /// role to the next eligible steward.
    #[test]
    fn deadlock_accepted_skips_the_epoch_steward_and_moves_to_the_next() {
        let mut e = founder();
        e.begin(Timestamp::ZERO);
        let eligible = e.queues.steward_eligibility(&e.members);
        let es = e
            .steward_list
            .epoch_steward(e.epoch, &eligible)
            .unwrap()
            .to_vec();
        drop(eligible);

        e.skip_silent_epoch_steward();
        let out = e.finish();

        assert!(out.events.iter().any(|ev| matches!(
            ev,
            Event::StewardSkipped { steward, .. } if steward.as_bytes() == es.as_slice()
        )));
        assert!(e.queues.skipped_stewards().contains(&es));

        let eligible = e.queues.steward_eligibility(&e.members);
        let next = e.steward_list.epoch_steward(e.epoch, &eligible).unwrap();
        assert_ne!(next, es.as_slice(), "the next eligible steward takes over");
    }
}
