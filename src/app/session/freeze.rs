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

use std::sync::{Arc, RwLock};

use tracing::{error, info};

use crate::{
    app::{
        ConversationState, DispatchOutcome, FreezeTimeoutStatus, LockExt, SessionRunner, UserError,
        session::runner::send_packet,
    },
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
    pub fn check_pending_join(arc: &Arc<RwLock<Self>>) -> Result<PendingJoinTick, UserError> {
        let (state, expired, conversation_name) = {
            let s = arc.read_or_err("session")?;
            (
                s.handle.current_state(),
                s.is_pending_join_expired(),
                s.conversation_name.clone(),
            )
        };
        if state != ConversationState::PendingJoin {
            return Ok(PendingJoinTick::NotPending);
        }
        if !expired {
            return Ok(PendingJoinTick::StillPending);
        }
        info!(conversation = %conversation_name, "pending join timed out");
        arc.read_or_err("session")?
            .emit_event(SessionEvent::Leaving);
        Ok(PendingJoinTick::Expired)
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
            let mut s = arc.write_or_err("session")?;

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

        arc.read_or_err("session")?
            .emit_event(SessionEvent::PhaseChange(selection_event));

        let (mut finalize_result, downward_cross, conversation_name) = {
            let mut s = arc.write_or_err("session")?;
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
            arc.read_or_err("session")?
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
                if let Some(packet) = outbound {
                    let transport = Arc::clone(arc.read_or_err("session")?.transport());
                    if let Err(e) = send_packet(&transport, packet) {
                        error!(conversation = %conversation_name, error = %e, "deferred welcome send failed");
                    }
                }

                // ConversationSync carries the steward list + timing + scores
                // to new joiners; send it only after the welcome they'll use
                // to catch up.
                if has_welcome {
                    let sync_packet_result = {
                        let mut s = arc.write_or_err("session")?;
                        let transport = Arc::clone(s.transport());
                        s.build_conversation_sync_packet()
                            .map(|opt| opt.map(|packet| (transport, packet)))
                    };
                    match sync_packet_result {
                        Ok(Some((transport, packet))) => {
                            if let Err(e) = send_packet(&transport, packet) {
                                error!(conversation = %conversation_name, error = %e, "conversation sync send failed");
                            } else {
                                info!(conversation = %conversation_name, "conversation sync sent");
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            error!(conversation = %conversation_name, error = %e, "conversation sync build failed");
                        }
                    }
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
                    let mut s = arc.write_or_err("session")?;

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
                arc.read_or_err("session")?
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
    ///
    /// Takes `&Arc<RwLock<Self>>` so the runner lock is released before
    /// awaiting on the transport for the steward's own candidate send.
    pub async fn check_member_freeze(arc: &Arc<RwLock<Self>>) -> Result<bool, UserError> {
        // Sync phase: under the runner write lock, run the inactivity
        // check and (for stewards) build the outbound candidate.
        let (transitioned, transport, outbound) = {
            let mut s = arc.write_or_err("session")?;
            let state = s.handle.current_state();
            if state == ConversationState::PendingJoin {
                return Ok(false);
            }

            let proposal_count = s.handle.conversation.approved_proposals_count();
            // Hold the freeze while an election is in flight — committing on
            // the known-stale list would just produce a NoCandidate.
            if s.handle.conversation.has_election_in_flight() {
                return Ok(false);
            }
            // Recovery uses the shorter retry inactivity window so we don't
            // burn another full epoch waiting for a steward to commit.
            let in_recovery =
                s.handle.is_in_recovery_mode() || s.handle.steward_list.retry_round() > 0;
            let inactivity = if in_recovery {
                s.handle.config.recovery_inactivity_duration
            } else {
                s.handle.config.commit_inactivity_duration
            };
            let freeze_event = s.check_steward_inactivity(proposal_count, inactivity);
            let Some(event) = freeze_event else {
                return Ok(false);
            };
            let epoch = s.handle.expect_mls()?.current_epoch()?;
            s.handle.conversation.ensure_freeze_round(epoch);

            let self_identity = Arc::clone(&s.self_identity);
            let app_id = Arc::clone(&s.app_id);
            let outbound = if s.handle.steward_list.is_steward(&self_identity) {
                match s.handle.create_commit_candidate(&self_identity, &app_id) {
                    Ok(packets) => packets,
                    Err(e) => {
                        error!(
                            conversation = %s.conversation_name,
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
                conversation = %s.conversation_name,
                approved = proposal_count,
                "steward inactivity transition"
            );

            s.emit_event(SessionEvent::PhaseChange(event));
            (true, Arc::clone(s.transport()), outbound)
        };

        // Async phase: release the lock before awaiting the transport.
        if let Some(message) = outbound {
            send_packet(&transport, message)?;
        }

        Ok(transitioned)
    }
}
