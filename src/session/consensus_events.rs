//! Consensus-event dispatch on `SessionRunner`. Triggered when the
//! hashgraph-like-consensus service reaches or fails consensus on a
//! proposal; compare with `inbound.rs` (transport-delivered packets).
//!
//! The handlers fan out into steward initiations (election, deadlock ECP,
//! score removals, buffered-update drains), each of which opens a follow-up
//! proposal.

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use prost::Message;
use tracing::{error, info};

use crate::{
    core::{
        ConsensusApplyResult, ConsensusPlugin, ConversationPluginsFactory, PeerScoringPlugin,
        ScoreOp, SessionEvent, StewardListPlugin, apply_consensus_result, emergency_score_ops,
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, conversation_update_request,
    },
    session::{ConversationState, SessionRunner, SessionTick, SessionError},
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Entry point from the consensus event bus: decode the proposal,
    /// apply the result to the conversation, and dispatch to the correct
    /// follow-up handler (election-accepted / election-rejected /
    /// emergency-scored).
    pub(crate) fn apply_consensus_outcome(
        &mut self,
        event: ConsensusEvent,
    ) -> Result<SessionTick, SessionError> {
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

        // Proposal resolved — any pending auto-vote timer is now moot.
        self.cancel_auto_vote(proposal_id);
        // Drop re-emissions from the consensus library (timeout-path
        // race) so we don't re-apply state or double-fire UI events.
        let already_applied = self
            .conversation
            .queues
            .is_consensus_outcome_applied(proposal_id);
        if already_applied {
            tracing::debug!(
                conversation = %self.conversation_id,
                proposal_id,
                "duplicate consensus outcome dropped"
            );
            return self.current_tick();
        }

        // Surface the decision before any effects so UI fanout sees it in
        // the same polling cycle as the state change that follows.
        self.emit_event(SessionEvent::ConsensusReached {
            proposal_id,
            approved,
            timestamp,
        });
        let consensus = self.consensus.clone();
        let conversation_id = self.conversation_id.clone();
        let scope = P::Scope::from(conversation_id.clone());
        let proposal = consensus.storage().get_proposal(&scope, proposal_id)?;
        // Decode once and thread the request through every downstream step.
        let request = ConversationUpdateRequest::decode(proposal.payload.as_slice())?;

        // The inactivity timer is self-started by `check_steward_inactivity`
        // on the next poll — no explicit notification needed here.
        info!(
            conversation = %conversation_id,
            proposal_id, approved, "consensus reached"
        );
        self.conversation
            .queues
            .mark_consensus_outcome_applied(proposal_id);
        let consensus_apply = apply_consensus_result(
            &mut self.conversation.queues,
            proposal_id,
            approved,
            &request,
        )?;

        // A commit candidate may have arrived before this approval landed (a
        // peer steward can reach consensus and broadcast the commit before we
        // apply our own vote). Now that the approved queue is populated, replay
        // any stashed candidate so the freeze round picks it up instead of
        // starting empty and forcing a needless reelection.
        self.conversation.replay_early_candidates()?;

        match consensus_apply {
            ConsensusApplyResult::NoAction => {}
            ConsensusApplyResult::ElectionAccepted(election) => {
                self.handle_election_accepted(election)?;
                return self.current_tick();
            }
            ConsensusApplyResult::ElectionRejected => {
                self.handle_election_rejected()?;
            }
            ConsensusApplyResult::RecoveryModeOpened => {
                self.conversation.enter_recovery_mode();
                self.start_freezing_and_emit()?;
            }
            ConsensusApplyResult::UrgentRemoval { target } => {
                self.start_freezing_and_emit()?;
                self.refresh_stewards_after_removal(&target)?;
            }
            ConsensusApplyResult::QueuedRemoval { target } => {
                self.refresh_stewards_after_removal(&target)?;
            }
            ConsensusApplyResult::RejectedMembership { target } => {
                self.conversation.queues.remove_pending_update(&target);
            }
        }

        // Consensus has settled — drop the deadline so tick_deadlines
        // doesn't fire a stale handle_consensus_timeout.
        self.unregister_consensus_timeout(proposal_id);

        let score_ops = emergency_score_ops(&request, approved);
        if !score_ops.is_empty() {
            self.handle_emergency_scored(proposal_id, &request, &score_ops)?;
        }

        self.current_tick()
    }

    /// Latest [`SessionTick`]. Terminal value of every
    /// `apply_consensus_outcome` exit path.
    fn current_tick(&self) -> Result<SessionTick, SessionError> {
        Ok(self.tick())
    }

    /// Emit a [`SessionEvent::PhaseChange`] for `transition`, if a state
    /// change occurred. Shared by the freeze / election / emergency paths.
    fn emit_phase_change(&self, transition: Option<ConversationState>) -> Result<(), SessionError> {
        if let Some(state) = transition {
            self.emit_event(SessionEvent::PhaseChange(state));
        }
        Ok(())
    }

    /// Enter Freezing immediately (bypassing the inactivity timer) and emit
    /// the resulting phase change. Called by `apply_consensus_outcome` for
    /// `UrgentRemoval` and `RecoveryModeOpened` outcomes that need an
    /// immediate commit.
    fn start_freezing_and_emit(&mut self) -> Result<(), SessionError> {
        let transition = self.start_freezing();
        self.emit_phase_change(transition)
    }

    /// When the removal target is a current steward, fire a fresh election
    /// in parallel so the next epoch keeps a healthy ES + BS.
    fn refresh_stewards_after_removal(&mut self, target: &[u8]) -> Result<(), SessionError> {
        if !self.conversation.steward_list.is_steward(target) {
            return Ok(());
        }
        if let Err(e) = self.initiate_steward_election(true) {
            info!(
                conversation = %self.conversation_id,
                error = %e,
                "post-removal steward-list refresh deferred"
            );
        }
        Ok(())
    }

    /// Accepted election: validate, install the new list, exit Reelection
    /// if we were in it, close any open recovery window, and drain
    /// buffered updates so the fresh epoch steward picks them up.
    fn handle_election_accepted(
        &mut self,
        election: StewardElectionProposal,
    ) -> Result<(), SessionError> {
        self.conversation.expect_mls()?;
        // Election proposals carry the candidate pool implicitly:
        // `proposed_stewards` is the full set the proposer sorted, so
        // `candidate_pool == proposed_stewards` for validation.
        let is_valid = self.conversation.steward_list.validate_proposed(
            &election.proposed_stewards,
            election.election_epoch,
            &election.proposed_stewards,
            election.retry_round,
        )?;
        if !is_valid {
            info!(
                conversation = %self.conversation_id,
                "steward election rejected: invalid list"
            );
            return Ok(());
        }

        self.conversation.steward_list.install_list(
            election.election_epoch,
            &election.proposed_stewards,
            election.proposed_stewards.len(),
            election.retry_round,
        )?;
        // `retry_round` stays > 0 until the next successful commit so
        // the immediate post-election inactivity check uses the
        // short retry window.
        self.conversation.exit_recovery_mode();
        let resumed_from_reelection =
            if self.conversation.current_state() == ConversationState::Reelection {
                Some(self.start_working())
            } else {
                None
            };
        self.emit_phase_change(resumed_from_reelection)?;
        info!(
            conversation = %self.conversation_id,
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            retry_round = election.retry_round,
            "steward election applied"
        );

        self.process_buffered_updates()
    }

    /// Rejected election: bump the retry round and retry under the max
    /// (idempotent), or escalate to a `Deadlock` ECP once exhausted.
    fn handle_election_rejected(&mut self) -> Result<(), SessionError> {
        self.conversation.steward_list.bump_retry();
        let round = self.conversation.steward_list.next_retry_round();
        let max = self.conversation.steward_list.max_retries();
        let conversation_id = self.conversation_id.clone();
        if round > max {
            info!(
                conversation = %conversation_id,
                round, max, "election retries exhausted; escalating to Layer 3"
            );
            if let Err(e) = self.initiate_deadlock_ecp() {
                error!(conversation = %conversation_id, error = %e, "Deadlock ECP filing failed");
                self.emit_event(SessionEvent::Error {
                    operation: "Reelection stuck".to_string(),
                    message: e.to_string(),
                });
            }
            return Ok(());
        }
        info!(
            conversation = %conversation_id,
            round, max, "steward election rejected, retrying"
        );
        if let Err(e) = self.initiate_steward_election(true) {
            info!(conversation = %conversation_id, error = %e, "election retry deferred");
        }
        Ok(())
    }

    /// Emergency proposal resolved: apply score ops, clear the
    /// pending-removal / pending-ECP buffers, lift the partial freeze (and
    /// exit Reelection if we landed there), then check for new
    /// below-threshold removals.
    fn handle_emergency_scored(
        &mut self,
        proposal_id: u32,
        request: &ConversationUpdateRequest,
        score_ops: &[ScoreOp],
    ) -> Result<(), SessionError> {
        // Events from this apply chain into the score-removal pass
        // below. The terminal `check_and_initiate_score_removals`
        // call covers it, so we drop the result here.
        let _ = self.conversation.scoring.apply_ops(score_ops);
        if let Some(conversation_update_request::Payload::EmergencyCriteria(ec)) = &request.payload
            && let Some(ev) = &ec.evidence
        {
            self.conversation
                .queues
                .remove_pending_removal(&ev.target_member_id);
        }

        self.conversation.queues.remove_emergency(proposal_id);
        let resumed_event = if self.conversation.current_state() == ConversationState::Reelection {
            Some(self.start_working())
        } else {
            None
        };
        self.emit_phase_change(resumed_event)?;

        if let Err(e) = self.check_and_initiate_score_removals() {
            error!(conversation = %self.conversation_id, error = %e, "score-removal check failed");
        }
        Ok(())
    }
}
