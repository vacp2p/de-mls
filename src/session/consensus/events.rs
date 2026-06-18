//! Applying resolved consensus outcomes to the conversation.

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use openmls_traits::signatures::Signer;
use prost::Message;
use tracing::{error, info};

use crate::{
    core::{
        ConsensusApplyResult, ConsensusPlugin, ConversationEvent, ConversationPluginsFactory,
        PeerScoringPlugin, ScoreOp, StewardListPlugin, apply_consensus_result, emergency_score_ops,
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, conversation_update_request,
    },
    session::{Conversation, ConversationError, ConversationState},
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Conversation<P, CP> {
    /// Apply one resolved outcome: surface the decision to the integrator,
    /// apply the queue effects, then run whatever follow-up the result
    /// calls for (election install/retry, freeze entry, emergency scoring).
    pub(crate) fn apply_consensus_outcome(
        &mut self,
        event: ConsensusEvent,
        signer: &impl Signer,
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
        let already_applied = self.queues.is_consensus_outcome_applied(proposal_id);
        if already_applied {
            tracing::debug!(
                conversation = %self.conversation_id,
                proposal_id,
                "duplicate consensus outcome dropped"
            );
            return Ok(());
        }

        // Emitted before any effects, so the integrator sees the decision
        // in the same polling cycle as the state changes it triggers.
        self.emit_event(ConversationEvent::ConsensusReached {
            proposal_id,
            approved,
            timestamp,
        });
        let scope = P::Scope::from(self.conversation_id.clone());
        let proposal = self.consensus.storage().get_proposal(&scope, proposal_id)?;
        let request = ConversationUpdateRequest::decode(proposal.payload.as_slice())?;

        // Nothing arms the inactivity timer here — the next poll's
        // inactivity check self-starts it once it sees approved work.
        info!(
            conversation = %self.conversation_id,
            proposal_id, approved, "consensus reached"
        );
        self.queues.mark_consensus_outcome_applied(proposal_id);
        let consensus_apply =
            apply_consensus_result(&mut self.queues, proposal_id, approved, &request)?;

        // A peer steward can reach consensus and broadcast its commit
        // candidate before our own outcome lands. Now that the approved
        // queue is populated, replay any stashed candidate so the freeze
        // round starts with it instead of empty (which would force a
        // needless reelection).
        self.replay_early_candidates()?;

        match consensus_apply {
            ConsensusApplyResult::NoAction => {}
            ConsensusApplyResult::ElectionAccepted(election) => {
                self.handle_election_accepted(election, signer)?;
            }
            ConsensusApplyResult::ElectionRejected => {
                self.handle_election_rejected(signer)?;
            }
            ConsensusApplyResult::RecoveryModeOpened => {
                self.enter_recovery_mode();
                self.start_freezing_and_emit();
            }
            ConsensusApplyResult::UrgentRemoval { target } => {
                self.start_freezing_and_emit();
                self.refresh_stewards_after_removal(&target, signer)?;
            }
            ConsensusApplyResult::QueuedRemoval { target } => {
                self.refresh_stewards_after_removal(&target, signer)?;
            }
            ConsensusApplyResult::RejectedMembership { target } => {
                self.queues.remove_pending_update(&target);
            }
        }

        // Empty for everything but emergency-criteria payloads.
        let score_ops = emergency_score_ops(&request, approved);
        if !score_ops.is_empty() {
            self.handle_emergency_scored(proposal_id, &request, &score_ops, signer)?;
        }

        Ok(())
    }

    /// Emit a [`ConversationEvent::PhaseChange`] if a transition occurred.
    fn emit_phase_change(&self, transition: Option<ConversationState>) {
        if let Some(state) = transition {
            self.emit_event(ConversationEvent::PhaseChange(state));
        }
    }

    /// Enter `Freezing` now, bypassing the inactivity timer — for outcomes
    /// that need a commit immediately rather than on the next timeout.
    fn start_freezing_and_emit(&mut self) {
        let transition = self.start_freezing();
        self.emit_phase_change(transition);
    }

    /// A removal that hits a current steward triggers a fresh election so
    /// the next epoch still has a live epoch + backup steward. Election
    /// rejection here is deferred, not fatal — the list heals on a later
    /// reconcile.
    fn refresh_stewards_after_removal(
        &mut self,
        target: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        if !self.steward_list.is_steward(target) {
            return Ok(());
        }
        if let Err(e) = self.initiate_steward_election(true, signer) {
            info!(
                conversation = %self.conversation_id,
                error = %e,
                "post-removal steward-list refresh deferred"
            );
        }
        Ok(())
    }

    /// Validate and install an accepted steward list, close any recovery
    /// window, resume from `Reelection` if that's where we were, and drain
    /// buffered updates so the fresh epoch steward proposes them.
    fn handle_election_accepted(
        &mut self,
        election: StewardElectionProposal,
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        self.expect_mls()?;
        // The proposal carries no separate candidate pool: `proposed_stewards`
        // is the full set the proposer sorted.
        let is_valid = self.steward_list.validate_proposed(
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

        self.steward_list.install_list(
            election.election_epoch,
            &election.proposed_stewards,
            election.proposed_stewards.len(),
            election.retry_round,
        )?;
        // `retry_round` stays > 0 until the next successful commit, so the
        // post-election inactivity check keeps the short recovery window.
        self.exit_recovery_mode();
        let resumed_from_reelection = if self.current_state() == ConversationState::Reelection {
            Some(self.start_working())
        } else {
            None
        };
        self.emit_phase_change(resumed_from_reelection);
        info!(
            conversation = %self.conversation_id,
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            retry_round = election.retry_round,
            "steward election applied"
        );

        self.process_buffered_updates(signer)
    }

    /// Bump the retry round and re-run the election, or escalate to a
    /// `Deadlock` emergency proposal once retries are exhausted.
    fn handle_election_rejected(&mut self, signer: &impl Signer) -> Result<(), ConversationError> {
        self.steward_list.bump_retry();
        let round = self.steward_list.next_retry_round();
        let max = self.steward_list.max_retries();
        if round > max {
            info!(
                conversation = %self.conversation_id,
                round, max, "election retries exhausted; escalating to Layer 3"
            );
            if let Err(e) = self.initiate_deadlock_ecp(signer) {
                error!(conversation = %self.conversation_id, error = %e, "Deadlock ECP filing failed");
                self.emit_event(ConversationEvent::Error {
                    operation: "Reelection stuck".to_string(),
                    message: e.to_string(),
                });
            }
            return Ok(());
        }
        info!(
            conversation = %self.conversation_id,
            round, max, "steward election rejected, retrying"
        );
        if let Err(e) = self.initiate_steward_election(true, signer) {
            info!(conversation = %self.conversation_id, error = %e, "election retry deferred");
        }
        Ok(())
    }

    /// Emergency proposal resolved: apply the score ops, clear the
    /// pending-removal buffer, lift the partial freeze (removing it from
    /// the emergency set), and resume from `Reelection` if the emergency
    /// put us there. Ends with a below-threshold sweep — the score ops may
    /// have pushed someone over the removal line.
    fn handle_emergency_scored(
        &mut self,
        proposal_id: u32,
        request: &ConversationUpdateRequest,
        score_ops: &[ScoreOp],
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        // The threshold-cross flag is dropped: the terminal
        // `check_and_initiate_score_removals` sweep below covers it.
        let _ = self.scoring.apply_ops(score_ops);
        if let Some(conversation_update_request::Payload::EmergencyCriteria(ec)) = &request.payload
            && let Some(ev) = &ec.evidence
        {
            self.queues.remove_pending_removal(&ev.target_member_id);
        }

        self.queues.remove_emergency(proposal_id);
        let resumed_event = if self.current_state() == ConversationState::Reelection {
            Some(self.start_working())
        } else {
            None
        };
        self.emit_phase_change(resumed_event);

        if let Err(e) = self.check_and_initiate_score_removals(signer) {
            error!(conversation = %self.conversation_id, error = %e, "score-removal check failed");
        }
        Ok(())
    }
}
