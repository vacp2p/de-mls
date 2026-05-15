//! Timer polls for pending-join expiry, freeze timeout, and steward inactivity.
//!
//! `check_pending_join` returns a [`PendingJoinTick`] so a polling caller can
//! distinguish "still pending" / "now joined" / "timed out". On `Expired`
//! the session has already emitted `Leaving`; the caller drives the
//! User-side cleanup via [`crate::app::User::finalize_self_leave`].
//!
//! `poll_freeze_status` returns the freeze-tick status alongside a
//! [`DispatchOutcome`] for the rare case where a commit applied during
//! the freeze fires `LeaveConversation`. Same handshake as
//! [`SessionRunner::dispatch_inbound_result`].

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{error, info};

use super::DispatchOutcome;

use crate::{
    app::{ConversationState, FreezeTimeoutStatus, SessionRunner, UserError},
    core::{
        ConsensusPlugin, ConversationPluginsFactory, FreezeFinalizeResult, FreezeOutcome,
        PeerScoringEvent, PeerScoringPlugin, ScoreEvent, ScoreOp, SessionEvent, StewardListPlugin,
    },
    ds::WELCOME_SUBTOPIC,
    mls_crypto::MlsService,
};

/// `true` iff `events` contains at least one downward threshold cross.
/// Used to chain into a score-removal pass after applying score ops.
fn has_downward_cross(events: &[PeerScoringEvent]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, PeerScoringEvent::ThresholdCrossedDown { .. }))
}

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
    /// [`crate::app::User::finalize_self_leave`] to drop the entry from
    /// the registry and broadcast removal.
    Expired,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Polling check for `PendingJoin`. Returns [`PendingJoinTick::Expired`]
    /// after emitting `SessionEvent::Leaving` once the pending-join window
    /// elapses; the caller handles registry-side cleanup.
    pub async fn check_pending_join(arc: &Arc<RwLock<Self>>) -> PendingJoinTick {
        let (state, expired, conversation_name) = {
            let s = arc.read().await;
            (
                s.handle.current_state(),
                s.is_pending_join_expired(),
                s.conversation_name.clone(),
            )
        };
        if state != ConversationState::PendingJoin {
            return PendingJoinTick::NotPending;
        }
        if !expired {
            return PendingJoinTick::StillPending;
        }
        info!(conversation = %conversation_name, "pending join timed out");
        arc.read().await.emit_event(SessionEvent::Leaving);
        PendingJoinTick::Expired
    }

    /// Poll tick for `Freezing`: drives Freezing → Selection once candidates
    /// are all in or the freeze window elapses, then finalises, dispatches
    /// the resulting [`crate::core::ProcessResult`], and returns the
    /// freeze status. The [`DispatchOutcome`] is `LeaveRequested` if the
    /// applied commit ejected the local member — the caller drives the
    /// User-side registry teardown.
    pub async fn poll_freeze_status(
        arc: &Arc<RwLock<Self>>,
    ) -> Result<(FreezeTimeoutStatus, DispatchOutcome), UserError> {
        let (has_proposals, selection_event) = {
            let mut s = arc.write().await;

            let state = s.handle.current_state();
            if state != ConversationState::Freezing {
                return Ok((FreezeTimeoutStatus::NotFreezing, DispatchOutcome::Done));
            }

            // Early selection: skip remaining freeze time if all expected
            // stewards have submitted candidates.
            let all_candidates_in =
                s.handle.steward_list.current_list().is_some_and(|list| {
                    s.handle.conversation.freeze_candidate_count() >= list.len()
                });

            if !all_candidates_in && !s.is_freeze_timed_out() {
                return Ok((FreezeTimeoutStatus::StillFreezing, DispatchOutcome::Done));
            }

            let event = s.start_selection();
            (s.handle.conversation.approved_proposals_count() > 0, event)
        };

        arc.read()
            .await
            .emit_event(SessionEvent::PhaseChange(selection_event));

        let (mut finalize_result, downward_cross, conversation_name) = {
            let mut s = arc.write().await;
            let allow_subset = s.handle.steward_list.config().allow_subset_candidates;
            let self_identity = Arc::clone(&s.self_identity);
            let app_id = Arc::clone(&s.app_id);
            let result = if s.handle.mls().is_some() {
                match s
                    .handle
                    .finalize_freeze_round(allow_subset, &app_id, &self_identity)
                {
                    Ok(result) => result,
                    Err(e) => {
                        error!(conversation = %s.conversation_name, error = %e, "freeze finalize failed");
                        FreezeFinalizeResult::default()
                    }
                }
            } else {
                FreezeFinalizeResult::default()
            };
            // Apply locally-observed score events before releasing the
            // runner lock. These come from dropped candidates in the
            // phase-3 loop (RFC §Peer Scoring: direct local observation,
            // no ECP needed). A downward threshold cross schedules a
            // removal-init pass below, after the lock drops.
            let cross = if !result.score_ops.is_empty() {
                let events = s.handle.scoring.apply_ops(&result.score_ops);
                has_downward_cross(&events)
            } else {
                false
            };
            (result, cross, s.conversation_name.clone())
        };

        if !finalize_result.committed_batch.is_empty() {
            arc.read()
                .await
                .emit_event(SessionEvent::CommitApplied(std::mem::take(
                    &mut finalize_result.committed_batch,
                )));
        }

        // Lock split is intentional: `check_and_initiate_score_removals`
        // re-acquires the runner write lock and calls `initiate_proposal`,
        // which `.await`s on the consensus service. Holding the runner
        // lock across that await would block other operations on this
        // conversation, so we drop the lock above before chaining.
        if downward_cross && let Err(e) = Self::check_and_initiate_score_removals(arc).await {
            error!(conversation = %conversation_name, error = %e, "score-removal check failed (freeze finalize)");
        }

        match finalize_result.outcome {
            FreezeOutcome::Applied { result, outbound } => {
                // Welcomes are deferred to here so joiners can't advance
                // epoch ahead of the steward.
                let has_welcome = outbound
                    .as_ref()
                    .is_some_and(|p| p.subtopic == WELCOME_SUBTOPIC);
                if let Some(packet) = outbound
                    && let Err(e) = arc.read().await.send_outbound(packet).await
                {
                    error!(conversation = %conversation_name, error = %e, "deferred welcome send failed");
                }

                // ConversationSync carries the steward list + timing + scores
                // to new joiners; send it only after the welcome they'll use
                // to catch up.
                if has_welcome && let Err(e) = arc.read().await.send_conversation_sync().await {
                    error!(conversation = %conversation_name, error = %e, "conversation sync send failed");
                }

                let outcome = match Self::dispatch_inbound_result(arc, result).await {
                    Ok(o) => o,
                    Err(e) => {
                        error!(conversation = %conversation_name, error = %e, "finalize result dispatch failed");
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
                let (transition_event, downward_cross) = {
                    let mut s = arc.write().await;

                    if has_proposals {
                        // Approved batch (and in-flight votes) survive so
                        // the recovered steward commits the same proposals
                        // once the next election lands.
                        let event = s.start_reelection();

                        // Local observation → direct peer-score penalty,
                        // no ECP round-trip. Each honest member records
                        // the same event independently; threshold-crossing
                        // removal still goes through SCORE_BELOW_THRESHOLD
                        // consensus in steward.rs.
                        let accuse_target = match s.handle.mls() {
                            Some(mls) => {
                                let violation_epoch = mls.current_epoch()?;
                                let self_identity: &[u8] = &s.self_identity;
                                let members = s.handle.conversation_members()?;
                                let eligible = s.handle.conversation.steward_eligibility(&members);
                                s.handle
                                    .steward_list
                                    .epoch_steward(violation_epoch, &eligible)
                                    .filter(|id| !id.is_empty() && *id != self_identity)
                                    .map(|id| id.to_vec())
                            }
                            None => None,
                        };
                        let cross = if let Some(steward_id) = accuse_target {
                            let events = s.handle.scoring.apply_op(&ScoreOp {
                                member_id: steward_id,
                                event: ScoreEvent::CensorshipInactivity,
                            });
                            has_downward_cross(&events)
                        } else {
                            false
                        };

                        (event, cross)
                    } else {
                        s.handle.conversation.clear_freeze_round();
                        let event = s.start_working();
                        (event, false)
                    }
                };

                if downward_cross && let Err(e) = Self::check_and_initiate_score_removals(arc).await
                {
                    error!(conversation = %conversation_name, error = %e, "score-removal check failed (freeze timeout)");
                }

                let entered_reelection = transition_event == ConversationState::Reelection;
                arc.read()
                    .await
                    .emit_event(SessionEvent::PhaseChange(transition_event));

                // Layer 2 recovery: regenerate the steward list. Only the
                // responsible proposer's call actually submits.
                if entered_reelection
                    && let Err(e) = Self::try_initiate_steward_election(arc, true, None).await
                {
                    info!(conversation = %conversation_name, error = %e, "recovery election deferred");
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
    /// their own commit candidate under the same lock; candidate-build
    /// failure is logged and the freeze transition proceeds (peers'
    /// candidates still get processed).
    pub async fn check_member_freeze(&mut self) -> Result<bool, UserError> {
        let state = self.handle.current_state();
        if state == ConversationState::PendingJoin {
            return Ok(false);
        }

        let proposal_count = self.handle.conversation.approved_proposals_count();
        // Hold the freeze while an election is in flight — committing on
        // the known-stale list would just produce a NoCandidate.
        if self.handle.conversation.has_election_in_flight() {
            return Ok(false);
        }
        // Recovery uses the shorter retry inactivity window so we don't
        // burn another full epoch waiting for a steward to commit.
        let in_recovery =
            self.handle.is_in_recovery_mode() || self.handle.steward_list.retry_round() > 0;
        let inactivity = if in_recovery {
            self.handle.config.recovery_inactivity_duration
        } else {
            self.handle.config.commit_inactivity_duration
        };
        let freeze_event = self.check_steward_inactivity(proposal_count, inactivity);
        if let Some(event) = freeze_event {
            let epoch = self.handle.expect_mls()?.current_epoch()?;
            self.handle.conversation.ensure_freeze_round(epoch);

            let self_identity = Arc::clone(&self.self_identity);
            let app_id = Arc::clone(&self.app_id);
            let outbound = if self.handle.steward_list.is_steward(&self_identity) {
                match self.handle.create_commit_candidate(&self_identity, &app_id) {
                    Ok(packets) => packets,
                    Err(e) => {
                        error!(
                            conversation = %self.conversation_name,
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
                conversation = %self.conversation_name,
                approved = proposal_count,
                "steward inactivity transition"
            );

            self.emit_event(SessionEvent::PhaseChange(event));
            if let Some(message) = outbound {
                self.send_outbound(message).await?;
            }
        }

        Ok(freeze_event.is_some())
    }
}
