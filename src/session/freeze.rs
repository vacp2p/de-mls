//! Timer polls for pending-join expiry, freeze timeout, and steward inactivity.
//!
//! `check_pending_join` returns a [`PendingJoinTick`] so a polling caller can
//! distinguish "still pending" / "now joined" / "timed out". On `Expired`
//! the session has already emitted `Leaving`; the caller drives the
//! User-side cleanup via `User::finalize_self_leave`.
//!
//! `poll_freeze_status` returns the freeze-tick status alongside a
//! [`DispatchOutcome`] for the rare case where a commit applied during
//! the freeze fires `LeaveConversation`. Same handshake as
//! [`SessionRunner::dispatch_inbound_result`].

use std::sync::Arc;

use tracing::{error, info};

use crate::{
    core::{
        ConsensusPlugin, ConversationPluginsFactory, FreezeFinalizeResult, FreezeOutcome,
        PeerScoringPlugin, ScoreEvent, ScoreOp, SessionEvent, StewardListPlugin,
    },
    mls_crypto::MlsService,
    session::{
        ConversationState, DispatchOutcome, FreezeTimeoutStatus, SessionError, SessionRunner,
    },
};

/// What [`SessionRunner::check_pending_join`] hands back to its polling
/// caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingJoinTick {
    /// Still in `PendingJoin`; caller should keep polling.
    StillPending,
    /// No longer in `PendingJoin` (joined or otherwise transitioned).
    NotPending,
    /// Pending-join window elapsed without a welcome. The session has
    /// emitted `Leaving`; the caller must follow up with
    /// `User::finalize_self_leave` to drop the entry from
    /// the registry and broadcast removal.
    Expired,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Polling check for `PendingJoin`. Returns [`PendingJoinTick::Expired`]
    /// after emitting `SessionEvent::Leaving` and cancelling timers once the
    /// pending-join window elapses; the caller handles registry-side cleanup.
    pub fn check_pending_join(&mut self) -> Result<PendingJoinTick, SessionError> {
        let state = self.conversation.current_state();
        if state != ConversationState::PendingJoin {
            return Ok(PendingJoinTick::NotPending);
        }
        if !self.is_pending_join_expired() {
            return Ok(PendingJoinTick::StillPending);
        }
        info!(conversation = %self.conversation_id, "pending join timed out");
        self.emit_event(SessionEvent::Leaving);
        self.cancel_all_auto_votes();
        Ok(PendingJoinTick::Expired)
    }

    /// Poll tick for `Freezing`: drives Freezing → Selection once candidates
    /// are all in or the freeze window elapses, then finalises, dispatches
    /// the resulting [`crate::core::ProcessResult`], and returns the
    /// freeze status. The [`DispatchOutcome`] is `LeaveRequested` if the
    /// applied commit ejected the local member — the caller drives the
    /// User-side registry teardown.
    pub fn poll_freeze_status(
        &mut self,
    ) -> Result<(FreezeTimeoutStatus, DispatchOutcome), SessionError> {
        let state = self.conversation.current_state();
        if state != ConversationState::Freezing {
            return Ok((FreezeTimeoutStatus::NotFreezing, DispatchOutcome::Done));
        }

        // Early selection: skip remaining freeze time if all expected
        // stewards have submitted candidates.
        let all_candidates_in = self
            .conversation
            .steward_list
            .current_list()
            .is_some_and(|list| self.conversation.queues.freeze_candidate_count() >= list.len());

        if !all_candidates_in && !self.is_freeze_timed_out() {
            return Ok((FreezeTimeoutStatus::StillFreezing, DispatchOutcome::Done));
        }

        let selection_event = self.start_selection();
        let has_proposals = self.conversation.queues.approved_proposals_count() > 0;
        self.emit_event(SessionEvent::PhaseChange(selection_event));

        let conversation_id = self.conversation_id.clone();
        let allow_subset = self
            .conversation
            .steward_list
            .config()
            .allow_subset_candidates;
        let self_member_id = Arc::clone(&self.self_member_id);
        let mut finalize_result = if self.conversation.mls().is_some() {
            match self
                .conversation
                .finalize_freeze_round(allow_subset, &self_member_id)
            {
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
            self.conversation
                .scoring
                .apply_ops(&finalize_result.score_ops)
        } else {
            false
        };

        if !finalize_result.committed_batch.is_empty() {
            self.emit_event(SessionEvent::CommitApplied(std::mem::take(
                &mut finalize_result.committed_batch,
            )));
        }

        // `check_and_initiate_score_removals` calls `initiate_proposal`. A
        // downward threshold cross during finalize schedules a removal pass.
        if downward_cross && let Err(e) = self.check_and_initiate_score_removals() {
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
                        self.build_conversation_sync_payload()?.unwrap_or_default()
                    };
                    self.emit_event(SessionEvent::WelcomeReady(welcome));
                }

                let outcome = match self.dispatch_inbound_result(result) {
                    Ok(o) => o,
                    Err(e) => {
                        error!(conversation = %conversation_id, error = %e, "finalize result dispatch failed");
                        DispatchOutcome::Done
                    }
                };
                return Ok((FreezeTimeoutStatus::Applied, outcome));
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
                    let accuse_target = match self.conversation.mls() {
                        Some(mls) => {
                            let violation_epoch = mls.current_epoch()?;
                            let members = mls.members()?;
                            let self_member_id: &[u8] = &self.self_member_id;
                            let eligible = self.conversation.queues.steward_eligibility(&members);
                            self.conversation
                                .steward_list
                                .epoch_steward(violation_epoch, &eligible)
                                .filter(|id| !id.is_empty() && *id != self_member_id)
                                .map(|id| id.to_vec())
                        }
                        None => None,
                    };
                    let cross = if let Some(steward_id) = accuse_target {
                        self.conversation.scoring.apply_op(&ScoreOp {
                            member_id: steward_id,
                            event: ScoreEvent::CensorshipInactivity,
                        })
                    } else {
                        false
                    };

                    (event, cross)
                } else {
                    self.conversation.queues.clear_freeze_round();
                    let event = self.start_working();
                    (event, false)
                };

                if downward_cross && let Err(e) = self.check_and_initiate_score_removals() {
                    error!(conversation = %conversation_id, error = %e, "score-removal check failed (freeze timeout)");
                }

                let entered_reelection = transition_event == ConversationState::Reelection;
                self.emit_event(SessionEvent::PhaseChange(transition_event));

                // Layer 2 recovery: regenerate the steward list. Only the
                // responsible proposer's call actually submits.
                if entered_reelection && let Err(e) = self.initiate_steward_election(true) {
                    info!(conversation = %conversation_id, error = %e, "recovery election deferred");
                }
            }
        }

        Ok((
            FreezeTimeoutStatus::TimedOut { has_proposals },
            DispatchOutcome::Done,
        ))
    }

    /// Drive the steward-inactivity check. Returns `true` exactly on the
    /// tick that transitions into Freezing; `false` while still waiting,
    /// outside Working, or when there's no approved work. Stewards build
    /// their own commit candidate too; candidate-build failure is logged
    /// and the freeze transition proceeds (peers' candidates still get
    /// processed).
    pub fn check_member_freeze(&mut self) -> Result<bool, SessionError> {
        let state = self.conversation.current_state();
        if state == ConversationState::PendingJoin {
            return Ok(false);
        }

        let proposal_count = self.conversation.queues.approved_proposals_count();
        // Hold the freeze while an election is in flight — committing on
        // the known-stale list would just produce a NoCandidate.
        if self.conversation.queues.has_election_in_flight() {
            return Ok(false);
        }
        // Recovery uses the shorter retry inactivity window so we don't
        // burn another full epoch waiting for a steward to commit.
        let in_recovery = self.conversation.is_in_recovery_mode()
            || self.conversation.steward_list.next_retry_round() > 0;
        let inactivity = if in_recovery {
            self.conversation.config.recovery_inactivity_duration
        } else {
            self.conversation.config.commit_inactivity_duration
        };
        let freeze_event = self.check_steward_inactivity(proposal_count, inactivity);
        let Some(event) = freeze_event else {
            return Ok(false);
        };
        let epoch = self.conversation.expect_mls()?.current_epoch()?;
        self.conversation.queues.start_freeze_round(epoch);

        let self_member_id = Arc::clone(&self.self_member_id);
        let outbound = if self.conversation.steward_list.is_steward(&self_member_id) {
            match self.conversation.create_commit_candidate(&self_member_id) {
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

        self.emit_event(SessionEvent::PhaseChange(event));

        if let Some(payload) = outbound {
            self.broadcast(payload);
        }

        Ok(true)
    }
}
