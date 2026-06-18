//! Unified per-cycle polling entry point for [`crate::Conversation`].
//!
//! [`Conversation::poll`] drives all time-based conversation paths in one call:
//! consensus-deadline ticks, freeze progression, steward-inactivity freeze
//! entry, and pending-join expiry. The sub-steps are private to this module —
//! the integrator calls `poll()` once per wakeup cycle and reacts to the
//! returned [`PollOutcome`].

use openmls_traits::signatures::Signer;
use std::{sync::Arc, time::Duration};

use prost::Message;
use tracing::{error, info, warn};

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationEvent,
    ConversationPluginsFactory, ConversationState, DispatchOutcome, FreezeFinalizeResult,
    FreezeOutcome, PeerScoringPlugin, ScoreEvent, ScoreOp, StewardListPlugin,
    mls_crypto::MlsService, protos::de_mls::messages::v1::AppMessage,
};

/// Summary returned by [`Conversation::poll`] after one polling pass.
#[derive(Debug, Clone)]
pub struct PollOutcome {
    /// Earliest deadline still pending after this pass. Forward to an
    /// external scheduler as the next wakeup hint.
    pub next_wakeup_in: Option<Duration>,
    /// `true` if this conversation should be torn down: either a `PendingJoin`
    /// expiry or a commit that ejected the local member. The integrator
    /// must remove the registry entry and clean up the consensus scope
    /// before its next polling cycle.
    pub leave_requested: bool,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Conversation<P, CP> {
    /// Drive one polling cycle: tick consensus deadlines, advance freeze
    /// state, check steward inactivity, and check pending-join expiry.
    ///
    /// Best-effort: each step runs regardless of whether the previous one
    /// failed; step errors are transient (a step that can't act this cycle
    /// retries on the next) and are logged rather than surfaced.
    ///
    /// Returns [`PollOutcome::leave_requested`] when the conversation is ready
    /// to be torn down; the integrator finalizes the leave.
    ///
    /// `signer` is the local member's MLS signer, threaded into the
    /// steward commit-candidate build and any auto-vote casts that fire
    /// this cycle.
    pub fn poll(&mut self, signer: &impl Signer) -> PollOutcome {
        let mut leave_requested = false;

        self.tick_deadlines(signer);

        match self.advance_freeze(signer) {
            Ok(DispatchOutcome::LeaveRequested) => leave_requested = true,
            Ok(_) => {}
            Err(e) => warn!(
                conversation = %self.conversation_id,
                error = %e,
                "advance_freeze error in poll"
            ),
        }

        if let Err(e) = self.start_freeze_on_inactivity(signer) {
            warn!(
                conversation = %self.conversation_id,
                error = %e,
                "inactivity-freeze error in poll"
            );
        }

        if self.check_pending_join() {
            leave_requested = true;
        }

        PollOutcome {
            next_wakeup_in: self.next_wakeup_in(),
            leave_requested,
        }
    }

    /// Polling check for `PendingJoin`: returns `true` once the pending-join
    /// window elapses without a welcome. By then the conversation has emitted
    /// `ConversationEvent::Leaving` and cancelled its timers; the integrator
    /// handles registry-side cleanup.
    fn check_pending_join(&mut self) -> bool {
        if self.current_state() != ConversationState::PendingJoin {
            return false;
        }
        if !self.is_pending_join_expired() {
            return false;
        }
        info!(conversation = %self.conversation_id, "pending join timed out");
        self.emit_event(ConversationEvent::Leaving);
        self.cancel_all_auto_votes();
        true
    }

    /// Drive the freeze phase forward. While `Freezing`, emits
    /// [`ConversationEvent::FreezeProgress`] as candidates arrive; once all
    /// expected candidates are in or the freeze window elapses, transitions
    /// to `Selection`, finalises the round, and dispatches the resulting
    /// [`crate::ProcessResult`]. Returns
    /// [`DispatchOutcome::LeaveRequested`] if the applied commit ejected the
    /// local member — `poll()` surfaces that as `leave_requested`.
    fn advance_freeze(
        &mut self,
        signer: &impl Signer,
    ) -> Result<DispatchOutcome, ConversationError> {
        let state = self.current_state();
        if state != ConversationState::Freezing {
            self.last_freeze_progress = None;
            return Ok(DispatchOutcome::Done);
        }

        // Early selection: skip remaining freeze time if all expected
        // stewards have submitted candidates.
        let all_candidates_in = self
            .steward_list
            .current_list()
            .is_some_and(|list| self.queues.freeze_candidate_count() >= list.len());

        if !all_candidates_in && !self.is_freeze_timed_out() {
            // Still freezing — surface candidate progress when it changes.
            let (received, expected) = self.freeze_candidate_count();
            if self.last_freeze_progress != Some((received, expected)) {
                self.last_freeze_progress = Some((received, expected));
                self.emit_event(ConversationEvent::FreezeProgress { received, expected });
            }
            return Ok(DispatchOutcome::Done);
        }
        self.last_freeze_progress = None;

        let selection_event = self.start_selection();
        let has_proposals = self.queues.approved_proposals_count() > 0;
        self.emit_event(ConversationEvent::PhaseChange(selection_event));

        let conversation_id = self.conversation_id.clone();
        let allow_subset = self.steward_list.config().allow_subset_candidates;
        let self_member_id = Arc::clone(&self.self_member_id);
        let mut finalize_result = if self.mls().is_some() {
            match self.finalize_freeze_round(allow_subset, &self_member_id) {
                Ok(result) => result,
                Err(e) => {
                    error!(conversation = %conversation_id, error = %e, "freeze finalize failed");
                    FreezeFinalizeResult::default()
                }
            }
        } else {
            FreezeFinalizeResult::default()
        };
        // Apply locally-observed score events. These come from dropped
        // candidates in the phase-3 loop (RFC §Peer Scoring: direct local
        // observation, no ECP needed). A downward threshold cross schedules
        // a removal-init pass below.
        let downward_cross = if !finalize_result.score_ops.is_empty() {
            self.scoring.apply_ops(&finalize_result.score_ops)
        } else {
            false
        };

        if !finalize_result.committed_batch.is_empty() {
            self.emit_event(ConversationEvent::CommitApplied(std::mem::take(
                &mut finalize_result.committed_batch,
            )));
        }

        // `check_and_initiate_score_removals` calls `initiate_proposal`. A
        // downward threshold cross during finalize schedules a removal pass.
        if downward_cross && let Err(e) = self.check_and_initiate_score_removals(signer) {
            error!(conversation = %conversation_id, error = %e, "score-removal check failed (freeze finalize)");
        }

        match finalize_result.outcome {
            FreezeOutcome::Applied { result, welcome } => {
                if let Some(mut welcome) = welcome {
                    // Bundle ConversationSync (steward list + timing +
                    // scores) into the welcome event so the integrator
                    // delivers both atomically. The joiner replays the
                    // sync payload through `handle_inbound` after MLS
                    // attaches.
                    //
                    // Reconcile the list to the just-merged epoch first, so a
                    // small group's sync carries the regenerated, joiner-
                    // inclusive list rather than the pre-commit one. A large
                    // group leaves the list for the post-commit election.
                    welcome.conversation_sync_bytes = {
                        let _ = self.reconcile_steward_list()?;
                        self.build_conversation_sync_payload(signer)?
                            .unwrap_or_default()
                    };
                    // Broadcast the welcome to the group so every member can deliver it to the
                    // joiners, then surface it locally as freshly minted.
                    let broadcast_payload = AppMessage::from(welcome.clone()).encode_to_vec();
                    self.emit_event(ConversationEvent::WelcomeReady {
                        welcome,
                        minted_locally: true,
                    });
                    self.broadcast(broadcast_payload);
                }

                let outcome = match self.dispatch_inbound_result(result, signer) {
                    Ok(o) => o,
                    Err(e) => {
                        error!(conversation = %conversation_id, error = %e, "finalize result dispatch failed");
                        DispatchOutcome::Done
                    }
                };
                Ok(outcome)
            }
            FreezeOutcome::NoCandidate => {
                // `accuse_target` is `Some` only when we had approved proposals
                // go unanswered *and* can attribute the miss to a live steward
                // other than ourselves. Self-penalties are skipped — the
                // node that failed to commit observes its own state directly
                // and doesn't need to record a ScoreOp against itself.
                let (transition_event, downward_cross) = if has_proposals {
                    // Approved batch (and in-flight votes) survive so
                    // the recovered steward commits the same proposals
                    // once the next election lands.
                    let event = self.start_reelection();

                    // Local observation → direct peer-score penalty,
                    // no ECP round-trip. Each honest member records
                    // the same event independently; threshold-crossing
                    // removal still goes through SCORE_BELOW_THRESHOLD
                    // consensus in steward.rs.
                    let accuse_target = match self.mls() {
                        Some(mls) => {
                            let violation_epoch = mls.current_epoch()?;
                            let members = mls.members()?;
                            let self_member_id: &[u8] = &self.self_member_id;
                            let eligible = self.queues.steward_eligibility(&members);
                            self.steward_list
                                .epoch_steward(violation_epoch, &eligible)
                                .filter(|id| !id.is_empty() && *id != self_member_id)
                                .map(|id| id.to_vec())
                        }
                        None => None,
                    };
                    let cross = if let Some(steward_id) = accuse_target {
                        self.scoring.apply_op(&ScoreOp {
                            member_id: steward_id,
                            event: ScoreEvent::CensorshipInactivity,
                        })
                    } else {
                        false
                    };

                    (event, cross)
                } else {
                    self.queues.clear_freeze_round();
                    let event = self.start_working();
                    (event, false)
                };

                if downward_cross && let Err(e) = self.check_and_initiate_score_removals(signer) {
                    error!(conversation = %conversation_id, error = %e, "score-removal check failed (freeze timeout)");
                }

                let entered_reelection = transition_event == ConversationState::Reelection;
                self.emit_event(ConversationEvent::PhaseChange(transition_event));

                // Layer 2 recovery: regenerate the steward list. Only the
                // responsible proposer's call actually submits.
                if entered_reelection && let Err(e) = self.initiate_steward_election(true, signer) {
                    info!(conversation = %conversation_id, error = %e, "recovery election deferred");
                }

                Ok(DispatchOutcome::Done)
            }
        }
    }

    /// Steward-inactivity freeze entry: once the inactivity timer fires with
    /// approved work pending, start the freeze round and transition into
    /// `Freezing`. Stewards build their own commit candidate too;
    /// candidate-build failure is logged and the freeze transition proceeds
    /// (peers' candidates still get processed). No-ops outside `Working`
    /// (via `check_steward_inactivity`) and while an election is in flight.
    fn start_freeze_on_inactivity(
        &mut self,
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        if self.current_state() == ConversationState::PendingJoin {
            return Ok(());
        }

        let proposal_count = self.queues.approved_proposals_count();
        // Hold the freeze while an election is in flight — committing on
        // the known-stale list would just produce a NoCandidate.
        if self.queues.has_election_in_flight() {
            return Ok(());
        }
        // Recovery uses the shorter retry inactivity window so we don't
        // burn another full epoch waiting for a steward to commit.
        let in_recovery = self.is_in_recovery_mode() || self.steward_list.next_retry_round() > 0;
        let inactivity = if in_recovery {
            self.config.recovery_inactivity_duration
        } else {
            self.config.commit_inactivity_duration
        };
        let Some(event) = self.check_steward_inactivity(proposal_count, inactivity) else {
            return Ok(());
        };
        let epoch = self.expect_mls()?.current_epoch()?;
        self.queues.start_freeze_round(epoch);

        let self_member_id = Arc::clone(&self.self_member_id);
        let outbound = if self.steward_list.is_steward(&self_member_id) {
            match self.create_commit_candidate(signer, &self_member_id) {
                Ok(payload) => payload,
                Err(e) => {
                    error!(
                        conversation = %self.conversation_id,
                        error = %e,
                        "commit candidate build failed"
                    );
                    None
                }
            }
        } else {
            None
        };

        info!(
            conversation = %self.conversation_id,
            approved = proposal_count,
            "steward inactivity transition"
        );

        self.emit_event(ConversationEvent::PhaseChange(event));

        if let Some(payload) = outbound {
            self.broadcast(payload);
        }

        Ok(())
    }
}
