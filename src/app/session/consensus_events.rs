//! Consensus-event dispatch on `SessionRunner`. Triggered when the
//! hashgraph-like-consensus service reaches or fails consensus on a
//! proposal; compare with `inbound.rs` (transport-delivered packets).
//!
//! All five handlers are associated functions taking
//! `Arc<RwLock<SessionRunner>>` because they fan out into steward
//! initiations (election, deadlock ECP, score removals, buffered-update
//! drains), each of which spawns a background proposal lifecycle.

use std::sync::{Arc, RwLock};

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use prost::Message;
use tracing::{error, info};

use super::lock::LockExt;

use crate::{
    app::{ConversationState, SessionRunner, UserError},
    core::{
        ConsensusApplyResult, ConsensusPlugin, ConversationPluginsFactory, PeerScoringPlugin,
        ProposalKind, ScoreOp, SessionEvent, StewardListPlugin, apply_consensus_result,
        emergency_score_ops, target_identity_of,
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
    pub async fn apply_consensus_outcome(
        arc: &Arc<RwLock<Self>>,
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
        arc.read_or_err("session")?.cancel_auto_vote(proposal_id);

        // Drop re-emissions from the consensus library (timeout-path race)
        // so we don't re-apply state or double-fire UI events.
        let already_applied = arc
            .read_or_err("session")?
            .handle
            .conversation
            .is_consensus_outcome_applied(proposal_id);
        if already_applied {
            let conv_name = arc.read_or_err("session")?.conversation_name.clone();
            tracing::debug!(
                conversation = %conv_name,
                proposal_id,
                "duplicate consensus outcome dropped"
            );
            return Ok(());
        }

        // Fetch payload from the per-conversation consensus storage.
        let (consensus, conversation_name) = {
            let s = arc.read_or_err("session")?;
            (s.consensus.clone(), s.conversation_name.clone())
        };
        let scope = P::Scope::from(conversation_name.clone());
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
                conversation = %s.conversation_name,
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
                return Self::handle_election_accepted(arc, election);
            }
            ConsensusApplyResult::RecoveryModeOpened => {
                arc.write_or_err("session")?.handle.enter_recovery_mode();
                Self::force_freezing_and_emit(arc)?;
            }
            ConsensusApplyResult::UrgentRemoval { target } => {
                Self::force_freezing_and_emit(arc)?;
                Self::refresh_stewards_after_removal(arc, &target)?;
            }
            ConsensusApplyResult::QueuedRemoval { target } => {
                Self::refresh_stewards_after_removal(arc, &target)?;
            }
        }

        if !approved && let Ok(req) = ConversationUpdateRequest::decode(payload.as_slice()) {
            if ProposalKind::of(&req).is_steward_election() {
                Self::handle_election_rejected(arc)?;
            } else if let Some(target) = target_identity_of(&req) {
                let target = target.to_vec();
                arc.write_or_err("session")?
                    .handle
                    .conversation
                    .remove_pending_update(&target);
            }
        }

        let score_ops = emergency_score_ops(&payload, approved);
        if !score_ops.is_empty() {
            Self::handle_emergency_scored(arc, proposal_id, &payload, &score_ops)?;
        }

        Ok(())
    }

    /// Bypass the inactivity timer and emit the resulting phase change.
    /// Called by [`Self::apply_consensus_outcome`] for `UrgentRemoval` and
    /// `RecoveryModeOpened` outcomes that need an immediate commit.
    fn force_freezing_and_emit(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let event = arc.write_or_err("session")?.force_freezing();
        if let Some(event) = event {
            arc.read_or_err("session")?
                .emit_event(SessionEvent::PhaseChange(event));
        }
        Ok(())
    }

    /// When the removal target is a current steward, fire a fresh election
    /// in parallel so the next epoch keeps a healthy ES + BS.
    fn refresh_stewards_after_removal(
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
        if let Err(e) = Self::try_initiate_steward_election(arc, true, Some(target)) {
            let conv_name = arc.read_or_err("session")?.conversation_name.clone();
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
    fn handle_election_accepted(
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
            let conv_name = arc.read_or_err("session")?.conversation_name.clone();
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
        if let Some(event) = resumed_from_reelection {
            arc.read_or_err("session")?
                .emit_event(SessionEvent::PhaseChange(event));
        }
        {
            let s = arc.read_or_err("session")?;
            info!(
                conversation = %s.conversation_name,
                epoch = election.election_epoch,
                stewards = election.proposed_stewards.len(),
                retry_round = election.retry_round,
                "steward election applied"
            );
        }

        Self::process_buffered_updates(arc)
    }

    /// Rejected election: bump the retry round and retry under the max
    /// (idempotent), or escalate to a `Deadlock` ECP once exhausted.
    fn handle_election_rejected(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let (round, max) = {
            let mut s = arc.write_or_err("session")?;
            let _events = s.handle.steward_list.bump_retry();
            (
                s.handle.steward_list.retry_round(),
                s.handle.steward_list.max_retries(),
            )
        };
        let conversation_name = arc.read_or_err("session")?.conversation_name.clone();
        if round > max {
            info!(
                conversation = %conversation_name,
                round, max, "election retries exhausted; escalating to Layer 3"
            );
            if let Err(e) = Self::try_initiate_deadlock_ecp(arc) {
                error!(conversation = %conversation_name, error = %e, "Deadlock ECP filing failed");
                arc.read_or_err("session")?.emit_event(SessionEvent::Error {
                    operation: "Reelection stuck".to_string(),
                    message: e.to_string(),
                });
            }
            return Ok(());
        }
        info!(
            conversation = %conversation_name,
            round, max, "steward election rejected, retrying"
        );
        if let Err(e) = Self::try_initiate_steward_election(arc, true, None) {
            info!(conversation = %conversation_name, error = %e, "election retry deferred");
        }
        Ok(())
    }

    /// Emergency proposal resolved: apply score ops, clear the
    /// pending-removal / pending-ECP buffers, lift the partial freeze (and
    /// exit Reelection if we landed there), then check for new
    /// below-threshold removals.
    fn handle_emergency_scored(
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
        if let Some(event) = resumed_event {
            arc.read_or_err("session")?
                .emit_event(SessionEvent::PhaseChange(event));
        }

        if let Err(e) = Self::check_and_initiate_score_removals(arc) {
            let conv_name = arc.read_or_err("session")?.conversation_name.clone();
            error!(conversation = %conv_name, error = %e, "score-removal check failed");
        }
        Ok(())
    }
}
