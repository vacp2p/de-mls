//! Consensus-event dispatch on `SessionRunner`. Triggered when the
//! hashgraph-like-consensus service reaches or fails consensus on a
//! proposal; compare with `inbound.rs` (transport-delivered packets).
//!
//! All five handlers are associated functions taking
//! `Arc<RwLock<SessionRunner>>` because they fan out into steward
//! initiations (election, deadlock ECP, score removals, buffered-update
//! drains), each of which opens a follow-up proposal that releases the
//! runner lock across its `.await` points.

use std::sync::{Arc, RwLock};

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use prost::Message;
use tracing::{error, info};

use crate::{
    app::{ConversationState, LockExt, SessionRunner, SessionTick, UserError},
    core::{
        ConsensusApplyResult, ConsensusPlugin, ConversationPluginsFactory, PeerScoringPlugin,
        ProposalKind, ScoreOp, SessionEvent, StewardListPlugin, apply_consensus_result,
        emergency_score_ops, target_member_id_of,
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, conversation_update_request,
    },
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Entry point from the consensus event bus: decode the proposal,
    /// apply the result to the conversation, and dispatch to the correct
    /// follow-up handler (election-accepted / election-rejected /
    /// emergency-scored).
    pub(crate) async fn apply_consensus_outcome(
        arc: &Arc<RwLock<Self>>,
        event: ConsensusEvent,
    ) -> Result<SessionTick, UserError> {
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

        let already_applied = {
            let mut s = arc.write_or_err("session")?;
            // Proposal resolved — any pending auto-vote timer is now moot.
            s.cancel_auto_vote(proposal_id);
            // Drop re-emissions from the consensus library (timeout-path
            // race) so we don't re-apply state or double-fire UI events.
            s.handle
                .conversation
                .is_consensus_outcome_applied(proposal_id)
        };
        if already_applied {
            let conv_name = arc.read_or_err("session")?.conversation_id.clone();
            tracing::debug!(
                conversation = %conv_name,
                proposal_id,
                "duplicate consensus outcome dropped"
            );
            return Self::current_tick(arc);
        }

        // Surface the decision before any effects so UI fanout sees it in
        // the same polling cycle as the state change that follows; grab the
        // payload-fetch handles under the same read guard.
        let (consensus, conversation_id) = {
            let s = arc.read_or_err("session")?;
            s.emit_event(SessionEvent::ConsensusReached {
                proposal_id,
                approved,
                timestamp,
            });
            (s.consensus.clone(), s.conversation_id.clone())
        };
        let scope = P::Scope::from(conversation_id.clone());
        let proposal = consensus
            .storage()
            .get_proposal(&scope, proposal_id)
            .await?;
        let payload = proposal.payload;

        // The inactivity timer is self-started by `check_steward_inactivity`
        // on the next poll — no explicit notification needed here.
        let consensus_apply = {
            let mut s = arc.write_or_err("session")?;
            info!(
                conversation = %s.conversation_id,
                proposal_id, approved, "consensus reached"
            );
            s.handle
                .conversation
                .mark_consensus_outcome_applied(proposal_id);
            apply_consensus_result(&mut s.handle.conversation, proposal_id, approved, &payload)?
        };

        match consensus_apply {
            ConsensusApplyResult::NoAction => {}
            ConsensusApplyResult::ElectionAccepted(election) => {
                Self::handle_election_accepted(arc, election).await?;
                return Self::current_tick(arc);
            }
            ConsensusApplyResult::RecoveryModeOpened => {
                arc.write_or_err("session")?.handle.enter_recovery_mode();
                Self::force_freezing_and_emit(arc)?;
            }
            ConsensusApplyResult::UrgentRemoval { target } => {
                Self::force_freezing_and_emit(arc)?;
                Self::refresh_stewards_after_removal(arc, &target).await?;
            }
            ConsensusApplyResult::QueuedRemoval { target } => {
                Self::refresh_stewards_after_removal(arc, &target).await?;
            }
        }

        if !approved && let Ok(req) = ConversationUpdateRequest::decode(payload.as_slice()) {
            if ProposalKind::of(&req).is_steward_election() {
                Self::handle_election_rejected(arc).await?;
            } else if let Some(target) = target_member_id_of(&req) {
                let target = target.to_vec();
                arc.write_or_err("session")?
                    .handle
                    .conversation
                    .remove_pending_update(&target);
            }
        }

        // Consensus has settled — drop the deadline so tick_deadlines
        // doesn't fire a stale handle_consensus_timeout.
        arc.write_or_err("session")?
            .unregister_consensus_timeout(proposal_id);

        let score_ops = emergency_score_ops(&payload, approved);
        if !score_ops.is_empty() {
            Self::handle_emergency_scored(arc, proposal_id, &payload, &score_ops).await?;
        }

        Self::current_tick(arc)
    }

    /// Latest [`SessionTick`] under a brief read guard. Terminal value of
    /// every `apply_consensus_outcome` exit path.
    fn current_tick(arc: &Arc<RwLock<Self>>) -> Result<SessionTick, UserError> {
        Ok(arc.read_or_err("session")?.tick())
    }

    /// Emit a [`SessionEvent::PhaseChange`] for `transition`, if a state
    /// change occurred. Shared by the freeze / election / emergency paths.
    fn emit_phase_change(
        arc: &Arc<RwLock<Self>>,
        transition: Option<ConversationState>,
    ) -> Result<(), UserError> {
        if let Some(state) = transition {
            arc.read_or_err("session")?
                .emit_event(SessionEvent::PhaseChange(state));
        }
        Ok(())
    }

    /// Bypass the inactivity timer and emit the resulting phase change.
    /// Called by `apply_consensus_outcome` for `UrgentRemoval` and
    /// `RecoveryModeOpened` outcomes that need an immediate commit.
    fn force_freezing_and_emit(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let transition = arc.write_or_err("session")?.force_freezing();
        Self::emit_phase_change(arc, transition)
    }

    /// When the removal target is a current steward, fire a fresh election
    /// in parallel so the next epoch keeps a healthy ES + BS.
    async fn refresh_stewards_after_removal(
        arc: &Arc<RwLock<Self>>,
        target: &[u8],
    ) -> Result<(), UserError> {
        let target_was_steward = arc
            .read_or_err("session")?
            .handle
            .steward_list
            .is_steward(target);
        if !target_was_steward {
            return Ok(());
        }
        if let Err(e) = Self::initiate_steward_election(arc, true).await {
            let conv_name = arc.read_or_err("session")?.conversation_id.clone();
            info!(
                conversation = %conv_name,
                error = %e,
                "post-removal steward-list refresh deferred"
            );
        }
        Ok(())
    }

    /// Accepted election: validate, install the new list, exit Reelection
    /// if we were in it, close any open recovery window, and drain
    /// buffered updates so the fresh epoch steward picks them up.
    async fn handle_election_accepted(
        arc: &Arc<RwLock<Self>>,
        election: StewardElectionProposal,
    ) -> Result<(), UserError> {
        let is_valid = {
            let s = arc.read_or_err("session")?;
            s.handle.expect_mls()?;
            // Election proposals carry the candidate pool implicitly:
            // `proposed_stewards` is the full set the proposer sorted, so
            // `candidate_pool == proposed_stewards` for validation.
            s.handle.steward_list.validate_proposed(
                &election.proposed_stewards,
                election.election_epoch,
                &election.proposed_stewards,
                election.retry_round,
            )?
        };
        if !is_valid {
            let conv_name = arc.read_or_err("session")?.conversation_id.clone();
            info!(
                conversation = %conv_name,
                "steward election rejected: invalid list"
            );
            return Ok(());
        }

        let resumed_from_reelection = {
            let mut s = arc.write_or_err("session")?;
            let _events = s.handle.steward_list.install_list(
                election.election_epoch,
                &election.proposed_stewards,
                election.proposed_stewards.len(),
                election.retry_round,
            )?;
            // `retry_round` stays > 0 until the next successful commit so
            // the immediate post-election inactivity check uses the
            // short retry window.
            s.handle.exit_recovery_mode();
            if s.handle.current_state() == ConversationState::Reelection {
                Some(s.start_working())
            } else {
                None
            }
        };
        Self::emit_phase_change(arc, resumed_from_reelection)?;
        {
            let s = arc.read_or_err("session")?;
            info!(
                conversation = %s.conversation_id,
                epoch = election.election_epoch,
                stewards = election.proposed_stewards.len(),
                retry_round = election.retry_round,
                "steward election applied"
            );
        }

        Self::process_buffered_updates(arc).await
    }

    /// Rejected election: bump the retry round and retry under the max
    /// (idempotent), or escalate to a `Deadlock` ECP once exhausted.
    async fn handle_election_rejected(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let (round, max) = {
            let mut s = arc.write_or_err("session")?;
            let _events = s.handle.steward_list.bump_retry();
            (
                s.handle.steward_list.retry_round(),
                s.handle.steward_list.max_retries(),
            )
        };
        let conversation_id = arc.read_or_err("session")?.conversation_id.clone();
        if round > max {
            info!(
                conversation = %conversation_id,
                round, max, "election retries exhausted; escalating to Layer 3"
            );
            if let Err(e) = Self::initiate_deadlock_ecp(arc).await {
                error!(conversation = %conversation_id, error = %e, "Deadlock ECP filing failed");
                arc.read_or_err("session")?.emit_event(SessionEvent::Error {
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
        if let Err(e) = Self::initiate_steward_election(arc, true).await {
            info!(conversation = %conversation_id, error = %e, "election retry deferred");
        }
        Ok(())
    }

    /// Emergency proposal resolved: apply score ops, clear the
    /// pending-removal / pending-ECP buffers, lift the partial freeze (and
    /// exit Reelection if we landed there), then check for new
    /// below-threshold removals.
    async fn handle_emergency_scored(
        arc: &Arc<RwLock<Self>>,
        proposal_id: u32,
        payload: &[u8],
        score_ops: &[ScoreOp],
    ) -> Result<(), UserError> {
        {
            let mut s = arc.write_or_err("session")?;
            // Events from this apply chain into the score-removal pass
            // below (after `handle_emergency_scored` returns into its
            // caller). The terminal `check_and_initiate_score_removals`
            // call covers it, so we only need to drop the events here.
            let _events = s.handle.scoring.apply_ops(score_ops);
            if let Ok(req) = ConversationUpdateRequest::decode(payload)
                && let Some(conversation_update_request::Payload::EmergencyCriteria(ec)) =
                    &req.payload
                && let Some(ev) = &ec.evidence
            {
                s.handle
                    .conversation
                    .resolve_pending_removal(&ev.target_member_id);
            }
        }

        let resumed_event = {
            let mut s = arc.write_or_err("session")?;
            s.handle.conversation.resolve_emergency(proposal_id);
            if s.handle.current_state() == ConversationState::Reelection {
                Some(s.start_working())
            } else {
                None
            }
        };
        Self::emit_phase_change(arc, resumed_event)?;

        if let Err(e) = Self::check_and_initiate_score_removals(arc).await {
            let conv_name = arc.read_or_err("session")?.conversation_id.clone();
            error!(conversation = %conv_name, error = %e, "score-removal check failed");
        }
        Ok(())
    }
}
