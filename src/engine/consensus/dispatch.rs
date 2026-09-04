//! [`Engine::handle_consensus_outcome`] is the acting half of outcome
//! handling: it reports the decision, runs [`apply_outcome`], and then
//! installs an election, skips a silent epoch steward, freezes for an
//! urgent removal, or applies emergency scores. The pure half — given the
//! decision and the request it carried, reshaping `EngineQueues` and naming
//! the follow-up this file still owes — lives in
//! [`crate::engine::consensus::apply`].

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use prost::Message;
use tracing::{debug, info, warn};

use crate::{
    ConversationError, ScoreOp,
    engine::{
        consensus::apply::{ApplyOutcome, apply_outcome},
        handle::Engine,
        store::EngineStore,
        types::{Event, MemberId},
    },
    peer_scoring::emergency_score_ops,
    protos::de_mls::messages::v1::{ConversationUpdateRequest, StewardElectionProposal},
};

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
    pub(crate) fn skip_silent_epoch_steward(&mut self) {
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

    /// Validate and install an accepted steward list.
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
        // it and authorizes the next steward's commit.
        if self.is_epoch_steward() {
            self.share_conversation_sync()
        } else {
            Ok(())
        }
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
