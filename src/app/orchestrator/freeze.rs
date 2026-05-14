//! Timer polls for pending-join expiry, freeze timeout, and steward inactivity.

use tracing::{error, info};

use crate::{
    app::{ConversationState, FreezeTimeoutStatus, User, UserError},
    core::{
        ConsensusPlugin, ConversationPluginsFactory, FreezeFinalizeResult, FreezeOutcome,
        PeerScoringPlugin, ScoreEvent, ScoreOp, SessionEvent, StewardListPlugin,
    },
    ds::WELCOME_SUBTOPIC,
    mls_crypto::MlsService,
};

use crate::app::orchestrator::has_downward_cross;

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    pub async fn check_pending_join(&self, conversation_name: &str) -> Result<bool, UserError> {
        let (state, expired) = match self
            .with_entry(conversation_name, |entry| {
                (
                    entry.handle.current_state(),
                    entry.is_pending_join_expired(),
                )
            })
            .await
        {
            Some(v) => v,
            None => return Ok(false),
        };

        if state != ConversationState::PendingJoin {
            return Ok(false);
        }

        if expired {
            info!(conversation = conversation_name, "pending join timed out");
            self.conversations.write().await.remove(conversation_name);
            self.cleanup_consensus_scope(conversation_name).await?;
            self.emit(conversation_name, SessionEvent::Leaving).await;
            let _ = self
                .lifecycle
                .send(crate::core::ConversationLifecycle::Removed(
                    conversation_name.to_string(),
                ));
            return Ok(false);
        }

        Ok(true)
    }

    /// Poll tick for `Freezing`: drives Freezing → Selection once candidates
    /// are all in or the freeze window elapses, then finalises and dispatches.
    pub async fn poll_freeze_status(
        &self,
        conversation_name: &str,
    ) -> Result<FreezeTimeoutStatus, UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;

        let (has_proposals, selection_event) = {
            let mut entry = entry_arc.write().await;

            let state = entry.handle.current_state();
            if state != ConversationState::Freezing {
                return Ok(FreezeTimeoutStatus::NotFreezing);
            }

            // Early selection: skip remaining freeze time if all expected
            // stewards have submitted candidates.
            let all_candidates_in = entry
                .handle
                .steward_list
                .current_list()
                .is_some_and(|list| {
                    entry.handle.conversation.freeze_candidate_count() >= list.len()
                });

            if !all_candidates_in && !entry.is_freeze_timed_out() {
                return Ok(FreezeTimeoutStatus::StillFreezing);
            }

            let event = entry.start_selection();
            (
                entry.handle.conversation.approved_proposals_count() > 0,
                event,
            )
        };

        self.emit(
            conversation_name,
            SessionEvent::PhaseChange(selection_event),
        )
        .await;

        let (mut finalize_result, downward_cross) = {
            let mut entry = entry_arc.write().await;
            let allow_subset = entry.handle.steward_list.config().allow_subset_candidates;
            let result = if entry.handle.mls().is_some() {
                match entry.handle.finalize_freeze_round(
                    allow_subset,
                    &self.app_id,
                    self.self_identity(),
                ) {
                    Ok(result) => result,
                    Err(e) => {
                        error!(conversation = conversation_name, error = %e, "freeze finalize failed");
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
                let events = entry.handle.scoring.apply_ops(&result.score_ops);
                has_downward_cross(&events)
            } else {
                false
            };
            (result, cross)
        };

        if !finalize_result.committed_batch.is_empty() {
            self.emit(
                conversation_name,
                SessionEvent::CommitApplied(std::mem::take(&mut finalize_result.committed_batch)),
            )
            .await;
        }

        // Lock split is intentional: `check_and_initiate_score_removals`
        // re-acquires the runner write lock and calls `initiate_proposal`,
        // which `.await`s on the consensus service. Holding the runner
        // lock across that await would block other operations on this
        // conversation, so we drop the lock above before chaining.
        if downward_cross
            && let Err(e) = self
                .check_and_initiate_score_removals(conversation_name)
                .await
        {
            error!(conversation = conversation_name, error = %e, "score-removal check failed (freeze finalize)");
        }

        match finalize_result.outcome {
            FreezeOutcome::Applied { result, outbound } => {
                // Welcomes are deferred to here so joiners can't advance
                // epoch ahead of the steward.
                let has_welcome = outbound
                    .as_ref()
                    .is_some_and(|p| p.subtopic == WELCOME_SUBTOPIC);
                if let Some(packet) = outbound {
                    if let Err(e) = self.send_outbound(packet).await {
                        error!(conversation = conversation_name, error = %e, "deferred welcome send failed");
                    }
                }

                // ConversationSync carries the steward list + timing + scores
                // to new joiners; send it only after the welcome they'll use
                // to catch up.
                if has_welcome {
                    if let Err(e) = self.send_conversation_sync(conversation_name).await {
                        error!(conversation = conversation_name, error = %e, "conversation sync send failed");
                    }
                }

                if let Err(e) = self
                    .dispatch_inbound_result(conversation_name, result)
                    .await
                {
                    error!(conversation = conversation_name, error = %e, "finalize result dispatch failed");
                }
                return Ok(FreezeTimeoutStatus::Applied);
            }
            FreezeOutcome::NoCandidate => {
                // `accuse_target` is `Some` only when we had approved proposals
                // go unanswered *and* can attribute the miss to a live steward
                // other than ourselves. Self-penalties are skipped — the
                // node that failed to commit observes its own state directly
                // and doesn't need to record a ScoreOp against itself.
                let (transition_event, downward_cross) = {
                    let mut entry = entry_arc.write().await;

                    if has_proposals {
                        // Approved batch (and in-flight votes) survive so
                        // the recovered steward commits the same proposals
                        // once the next election lands.
                        let event = entry.start_reelection();

                        // Local observation → direct peer-score penalty,
                        // no ECP round-trip. Each honest member records
                        // the same event independently; threshold-crossing
                        // removal still goes through SCORE_BELOW_THRESHOLD
                        // consensus in steward.rs.
                        let accuse_target = match entry.handle.mls() {
                            Some(mls) => {
                                let violation_epoch = mls.current_epoch()?;
                                let self_identity = self.self_identity();
                                let members = entry.handle.conversation_members()?;
                                let eligible =
                                    entry.handle.conversation.steward_eligibility(&members);
                                entry
                                    .handle
                                    .steward_list
                                    .epoch_steward(violation_epoch, &eligible)
                                    .filter(|id| !id.is_empty() && *id != self_identity)
                                    .map(|id| id.to_vec())
                            }
                            None => None,
                        };
                        let cross = if let Some(steward_id) = accuse_target {
                            let events = entry.handle.scoring.apply_op(&ScoreOp {
                                member_id: steward_id,
                                event: ScoreEvent::CensorshipInactivity,
                            });
                            has_downward_cross(&events)
                        } else {
                            false
                        };

                        (event, cross)
                    } else {
                        entry.handle.conversation.clear_freeze_round();
                        let event = entry.start_working();
                        (event, false)
                    }
                };

                if downward_cross
                    && let Err(e) = self
                        .check_and_initiate_score_removals(conversation_name)
                        .await
                {
                    error!(conversation = conversation_name, error = %e, "score-removal check failed (freeze timeout)");
                }

                let entered_reelection = transition_event == ConversationState::Reelection;
                self.emit(
                    conversation_name,
                    SessionEvent::PhaseChange(transition_event),
                )
                .await;

                // Layer 2 recovery: regenerate the steward list. Only the
                // responsible proposer's call actually submits.
                if entered_reelection
                    && let Err(e) = self
                        .try_initiate_steward_election(conversation_name, true, None)
                        .await
                {
                    info!(conversation = conversation_name, error = %e, "recovery election deferred");
                }
            }
        }

        Ok(FreezeTimeoutStatus::TimedOut { has_proposals })
    }

    pub async fn check_member_freeze(&self, conversation_name: &str) -> Result<bool, UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let mut entry = entry_arc.write().await;

        let state = entry.handle.current_state();
        if state == ConversationState::PendingJoin {
            return Ok(false);
        }

        let proposal_count = entry.handle.conversation.approved_proposals_count();
        // Hold the freeze while an election is in flight — committing on
        // the known-stale list would just produce a NoCandidate.
        if entry.handle.conversation.has_election_in_flight() {
            return Ok(false);
        }
        // Recovery uses the shorter retry inactivity window so we don't
        // burn another full epoch waiting for a steward to commit.
        let in_recovery =
            entry.handle.is_in_recovery_mode() || entry.handle.steward_list.retry_round() > 0;
        let inactivity = if in_recovery {
            entry.handle.config.recovery_inactivity_duration
        } else {
            entry.handle.config.commit_inactivity_duration
        };
        let freeze_event = entry.check_steward_inactivity(proposal_count, inactivity);
        if let Some(event) = freeze_event {
            let epoch = entry.handle.expect_mls()?.current_epoch()?;
            entry.handle.conversation.ensure_freeze_round(epoch);

            // Stewards build their own candidate under the same lock.
            // Candidate-build failure must not block the freeze transition —
            // peers' candidates still get processed.
            let self_identity = self.self_identity();
            let outbound = if entry.handle.steward_list.is_steward(self_identity) {
                match entry
                    .handle
                    .create_commit_candidate(self_identity, &self.app_id)
                {
                    Ok(packets) => packets,
                    Err(e) => {
                        error!(
                            conversation = conversation_name,
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
                conversation = conversation_name,
                approved = proposal_count,
                "steward inactivity transition"
            );

            // Drop lock before any async calls.
            drop(entry);
            self.emit(conversation_name, SessionEvent::PhaseChange(event))
                .await;
            if let Some(message) = outbound {
                self.send_outbound(message).await?;
            }
        }

        Ok(freeze_event.is_some())
    }
}
