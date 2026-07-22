//! Running the follow-up once a proposal resolves: installing an elected
//! roster, retrying or escalating a failed election, entering freeze for a
//! commit, and applying emergency score changes.

use std::error::Error as StdError;

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use openmls_traits::{OpenMlsProvider, signatures::Signer, storage::StorageProvider};
use prost::Message;
use tracing::{error, info};

use crate::{
    ConsensusApplyResult, ConsensusPlugin, Conversation, ConversationError, ConversationEvent,
    ConversationState, PeerScoreStorage, ScoreOp, WallClock, apply_consensus_result,
    emergency_score_ops,
    protos::de_mls::messages::v1::{ConversationUpdateRequest, StewardElectionProposal},
};

impl<Cp, Sc, Wc> Conversation<Cp, Sc, Wc>
where
    Cp: ConsensusPlugin,
    Sc: PeerScoreStorage,
    Wc: WallClock,
{
    /// Handle one resolved decision: tell the integrator, update the proposal
    /// queues via [`apply_consensus_result`], then run the follow-up it asks
    /// for — installing an election, entering freeze, applying emergency scores.
    pub(crate) fn handle_consensus_outcome<Pr>(
        &mut self,
        provider: &Pr,
        event: ConsensusEvent,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
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
        let scope = self.conversation_id.clone();
        let proposal = self
            .services
            .consensus
            .storage()
            .get_proposal(&scope, proposal_id)?;
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
        // queue is populated, replay any stashed candidate so the commit
        // round starts with it instead of empty (which would force a
        // needless reelection).
        self.replay_early_candidates()?;

        match consensus_apply {
            ConsensusApplyResult::NoAction => {}
            ConsensusApplyResult::ElectionAccepted(election) => {
                self.handle_election_accepted(provider, election, signer)?;
            }
            ConsensusApplyResult::ElectionRejected => {
                self.advance_election_retry(provider, signer)?;
            }
            ConsensusApplyResult::RecoveryModeOpened => {
                // Open the collection window and notify the integrator. Minting a
                // recovery candidate is the app's call (`commit_in_recovery`);
                // `advance_freezing` only selects among candidates already
                // broadcast, so nothing is minted here.
                self.enter_recovery_mode();
                self.start_freezing_and_emit();
                self.emit_event(ConversationEvent::RecoveryModeOpened);
            }
            ConsensusApplyResult::UrgentRemoval { target } => {
                // A steward-gated removal: enter freezing and mint it now.
                if let Some(event) = self.start_freezing() {
                    self.on_freeze_entered(provider, signer, event)?;
                }
                self.refresh_stewards_after_removal(provider, &target, signer)?;
            }
            ConsensusApplyResult::QueuedRemoval { target } => {
                self.refresh_stewards_after_removal(provider, &target, signer)?;
            }
            ConsensusApplyResult::RejectedMembership { target } => {
                self.queues.remove_pending_update(&target);
            }
        }

        // Empty for everything but emergency-criteria payloads.
        let score_ops = emergency_score_ops(&request, approved);
        if !score_ops.is_empty() {
            self.handle_emergency_scored(provider, proposal_id, &score_ops, signer)?;
        }

        Ok(())
    }

    /// Emit a [`ConversationEvent::PhaseChange`] if a transition occurred.
    fn emit_phase_change(&self, transition: Option<ConversationState>) {
        if let Some(state) = transition {
            self.emit_event(ConversationEvent::PhaseChange(state));
        }
    }

    /// Enter `Freezing` now, bypassing the inactivity timer, and announce it.
    fn start_freezing_and_emit(&mut self) {
        let transition = self.start_freezing();
        self.emit_phase_change(transition);
    }

    /// A removal that hits a current steward triggers a fresh election so
    /// the next epoch still has a live epoch + backup steward. Election
    /// rejection here is deferred, not fatal — the list heals on a later
    /// reconcile.
    fn refresh_stewards_after_removal<Pr>(
        &mut self,
        provider: &Pr,
        target: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if !self.services.steward_list.is_steward(target) {
            return Ok(());
        }
        if let Err(e) = self.initiate_steward_election(provider, true, signer) {
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
    fn handle_election_accepted<Pr>(
        &mut self,
        provider: &Pr,
        election: StewardElectionProposal,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        // Recompute the list from our own membership view, not the payload, so a
        // biased or tampered list is rejected (RFC anti-bias).
        let is_valid = self.validate_election_list(&election)?;
        if !is_valid {
            info!(
                conversation = %self.conversation_id,
                "steward election rejected: invalid list"
            );
            return Ok(());
        }

        self.services.steward_list.install_list(
            election.election_epoch,
            &election.proposed_stewards,
            election.proposed_stewards.len(),
            election.retry_round,
        )?;
        self.exit_recovery_mode();
        let resumed_from_reelection = if self.current_state() == ConversationState::Reelection {
            // The approved work this conversation froze over is a recovery
            // continuation: flag it so the app commits on the short recovery
            // window (via `in_recovery_posture`) rather than a fresh full
            // commit-inactivity epoch. `retry_round > 0` covers only bumped
            // rounds — a round-0 recovery election would otherwise wait a full
            // commit-inactivity epoch.
            self.timing.reelection_recovered = true;
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

        self.process_buffered_updates(provider, signer)
    }

    /// Emergency proposal resolved: apply the score ops, lift the partial
    /// freeze (removing it from the emergency set), and resume from
    /// `Reelection` if the emergency put us there. Ends with a below-threshold
    /// sweep — the score ops may have pushed someone over the removal line.
    fn handle_emergency_scored<Pr>(
        &mut self,
        provider: &Pr,
        proposal_id: u32,
        score_ops: &[ScoreOp],
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        self.services.scoring.apply_ops(score_ops)?;
        self.queues.remove_emergency(proposal_id);
        let resumed_event = if self.current_state() == ConversationState::Reelection {
            Some(self.start_working())
        } else {
            None
        };
        self.emit_phase_change(resumed_event);

        if let Err(e) = self.check_and_initiate_score_removals(provider, signer) {
            error!(conversation = %self.conversation_id, error = %e, "score-removal check failed");
        }
        Ok(())
    }
}
