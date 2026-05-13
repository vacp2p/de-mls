//! Consensus-event dispatch — the other side of the User's inbound edge,
//! triggered when the hashgraph-like-consensus service reaches or fails
//! consensus on a proposal. Compare with `inbound.rs` which handles
//! transport-delivered packets.

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use prost::Message;
use tracing::{error, info};

use crate::{
    app::{ConversationState, User, UserError},
    core::{
        ConsensusPlugin, ConversationPluginsFactory, PeerScoringPlugin, ProposalKind, ScoreOp,
        StewardListPlugin, apply_consensus_result, emergency_score_ops, target_identity_of,
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, conversation_update_request,
    },
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Entry point from the consensus service: decode the proposal, apply the
    /// result to the conversation, and dispatch to the correct follow-up handler
    /// (election-accepted / election-rejected / emergency-scored).
    pub async fn apply_consensus_outcome(
        &mut self,
        conversation_name: &str,
        event: ConsensusEvent,
    ) -> Result<(), UserError> {
        let (proposal_id, approved) = match &event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
                ..
            } => (*proposal_id, *result),
            ConsensusEvent::ConsensusFailed { proposal_id, .. } => (*proposal_id, false),
        };

        // Proposal resolved — any pending auto-vote timer for it is moot.
        self.cancel_auto_vote(conversation_name, proposal_id).await;

        // Drop re-emissions from the consensus library (timeout-path race)
        // so we don't re-apply state or double-fire UI events.
        let already_applied = self
            .with_entry(conversation_name, |entry| {
                entry
                    .handle
                    .conversation
                    .is_consensus_outcome_applied(proposal_id)
            })
            .await
            .unwrap_or(false);
        if already_applied {
            tracing::debug!(
                conversation = conversation_name,
                proposal_id,
                "duplicate consensus outcome dropped"
            );
            return Ok(());
        }

        // Fetch payload from consensus service.
        let scope = P::Scope::from(conversation_name.to_string());
        let proposal = self
            .consensus_service
            .storage()
            .get_proposal(&scope, proposal_id)
            .await?;
        let payload = proposal.payload;

        // The inactivity timer is self-started by `check_steward_inactivity`
        // on the next poll — no explicit notification needed here.
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let consensus_apply = {
            let mut entry = entry_arc.write().await;
            info!(
                conversation = conversation_name,
                proposal_id, approved, "consensus reached"
            );
            entry
                .handle
                .conversation
                .mark_consensus_outcome_applied(proposal_id);
            apply_consensus_result(
                &mut entry.handle.conversation,
                proposal_id,
                approved,
                &payload,
            )?
        };

        if let Some(election) = consensus_apply.election {
            return self
                .handle_election_accepted(conversation_name, election)
                .await;
        }

        if consensus_apply.enter_recovery_mode {
            if let Some(entry_arc) = self.lookup_entry(conversation_name).await {
                entry_arc.write().await.handle.enter_recovery_mode();
            }
        }

        if consensus_apply.force_freezing {
            // Bypass the inactivity timer so the urgent commit fires now.
            let event = match self.lookup_entry(conversation_name).await {
                Some(entry_arc) => entry_arc.write().await.force_freezing(),
                None => None,
            };
            if let Some(event) = event {
                self.handler.on_phase_change(conversation_name, event).await;
            }
        }

        if let Some(target) = &consensus_apply.queued_remove_target {
            self.refresh_stewards_after_removal(conversation_name, target)
                .await;
        }

        if !approved && let Ok(req) = ConversationUpdateRequest::decode(payload.as_slice()) {
            if ProposalKind::of(&req).is_steward_election() {
                self.handle_election_rejected(conversation_name).await;
            } else if let Some(target) = target_identity_of(&req) {
                let target = target.to_vec();
                if let Some(entry_arc) = self.lookup_entry(conversation_name).await {
                    let mut entry = entry_arc.write().await;
                    entry.handle.conversation.remove_pending_update(&target);
                }
            }
        }

        let score_ops = emergency_score_ops(&payload, approved);
        if !score_ops.is_empty() {
            self.handle_emergency_scored(conversation_name, proposal_id, &payload, &score_ops)
                .await?;
        }

        Ok(())
    }

    /// When the removal target is a current steward, fire a fresh election
    /// in parallel so the next epoch keeps a healthy ES + BS.
    async fn refresh_stewards_after_removal(&self, conversation_name: &str, target: &[u8]) {
        let target_was_steward = self
            .with_entry(conversation_name, |entry| {
                entry.handle.steward_list.is_steward(target)
            })
            .await
            .unwrap_or(false);
        if !target_was_steward {
            return;
        }
        if let Err(e) = self
            .try_initiate_steward_election(conversation_name, true, Some(target))
            .await
        {
            info!(
                conversation = conversation_name,
                error = %e,
                "post-removal steward-list refresh deferred"
            );
        }
    }

    /// Accepted election: validate, install the new list, exit Reelection
    /// if we were in it, close any open recovery window, and drain
    /// buffered updates so the fresh epoch steward picks them up.
    async fn handle_election_accepted(
        &self,
        conversation_name: &str,
        election: StewardElectionProposal,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;

        let is_valid = {
            let entry = entry_arc.read().await;
            entry.handle.expect_mls()?;
            // Election proposals carry the candidate pool implicitly:
            // `proposed_stewards` is the full set the proposer sorted, so
            // `candidate_pool == proposed_stewards` for validation.
            entry.handle.steward_list.validate_proposed(
                &election.proposed_stewards,
                election.election_epoch,
                &election.proposed_stewards,
                election.retry_round,
            )?
        };
        if !is_valid {
            info!(
                conversation = conversation_name,
                "steward election rejected: invalid list"
            );
            return Ok(());
        }

        let resumed_from_reelection = {
            let mut entry = entry_arc.write().await;
            let _events = entry.handle.steward_list.install_list(
                election.election_epoch,
                &election.proposed_stewards,
                election.proposed_stewards.len(),
                election.retry_round,
            )?;
            // `retry_round` stays > 0 until the next successful commit so
            // the immediate post-election inactivity check uses the
            // short retry window.
            entry.handle.exit_recovery_mode();
            if entry.handle.current_state() == ConversationState::Reelection {
                Some(entry.start_working())
            } else {
                None
            }
        };
        if let Some(event) = resumed_from_reelection {
            self.handler.on_phase_change(conversation_name, event).await;
        }
        info!(
            conversation = conversation_name,
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            retry_round = election.retry_round,
            "steward election applied"
        );

        self.process_buffered_updates(conversation_name).await
    }

    /// Rejected election: bump the retry round and retry under the max
    /// (idempotent), or escalate to a `Deadlock` ECP once exhausted.
    async fn handle_election_rejected(&self, conversation_name: &str) {
        let (round, max) = match self.lookup_entry(conversation_name).await {
            Some(entry_arc) => {
                let mut entry = entry_arc.write().await;
                let _events = entry.handle.steward_list.bump_retry();
                (
                    entry.handle.steward_list.retry_round(),
                    entry.handle.steward_list.max_retries(),
                )
            }
            None => return,
        };
        if round > max {
            info!(
                conversation = conversation_name,
                round, max, "election retries exhausted; escalating to Layer 3"
            );
            if let Err(e) = self.try_initiate_deadlock_ecp(conversation_name).await {
                error!(conversation = conversation_name, error = %e, "Deadlock ECP filing failed");
                self.handler
                    .on_error(conversation_name, "Reelection stuck", &e.to_string())
                    .await;
            }
            return;
        }
        info!(
            conversation = conversation_name,
            round, max, "steward election rejected, retrying"
        );
        if let Err(e) = self
            .try_initiate_steward_election(conversation_name, true, None)
            .await
        {
            info!(conversation = conversation_name, error = %e, "election retry deferred");
        }
    }

    /// Emergency proposal resolved: apply score ops, clear the
    /// pending-removal / pending-ECP buffers, lift the partial freeze (and
    /// exit Reelection if we landed there), then check for new
    /// below-threshold removals.
    async fn handle_emergency_scored(
        &self,
        conversation_name: &str,
        proposal_id: u32,
        payload: &[u8],
        score_ops: &[ScoreOp],
    ) -> Result<(), UserError> {
        if let Some(entry_arc) = self.lookup_entry(conversation_name).await {
            let mut entry = entry_arc.write().await;
            // Events from this apply chain into the score-removal pass
            // below (after `handle_emergency_scored` returns into its
            // caller). The terminal `check_and_initiate_score_removals`
            // call covers it, so we only need to drop the events here.
            let _events = entry.handle.scoring.apply_ops(score_ops);
            if let Ok(req) = ConversationUpdateRequest::decode(payload)
                && let Some(conversation_update_request::Payload::EmergencyCriteria(ec)) =
                    &req.payload
                && let Some(ev) = &ec.evidence
            {
                entry
                    .handle
                    .conversation
                    .resolve_pending_removal(&ev.target_member_id);
            }
        }

        let resumed_event = match self.lookup_entry(conversation_name).await {
            Some(entry_arc) => {
                let mut entry = entry_arc.write().await;
                entry.handle.conversation.resolve_emergency(proposal_id);
                if entry.handle.current_state() == ConversationState::Reelection {
                    Some(entry.start_working())
                } else {
                    None
                }
            }
            None => None,
        };
        if let Some(event) = resumed_event {
            self.handler.on_phase_change(conversation_name, event).await;
        }

        if let Err(e) = self
            .check_and_initiate_score_removals(conversation_name)
            .await
        {
            error!(conversation = conversation_name, error = %e, "score-removal check failed");
        }
        Ok(())
    }
}
