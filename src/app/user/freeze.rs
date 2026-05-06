//! Timer polls for pending-join expiry, freeze timeout, and steward inactivity.

use tracing::{error, info};

use crate::{
    app::{FreezeTimeoutStatus, GroupState, StateChangeHandler, User, UserError},
    core::{
        DeMlsProvider, FreezeFinalizeResult, FreezeOutcome, GroupEventHandler, PeerScoringPlugin,
        ScoreEvent, ScoreOp, create_commit_candidate, finalize_freeze_round, group_members,
    },
    ds::WELCOME_SUBTOPIC,
    mls_crypto::{IdentityProvider, MlsService},
};

use super::has_downward_cross;

impl<
    P: DeMlsProvider,
    M: MlsService,
    Sc: PeerScoringPlugin,
    H: GroupEventHandler + 'static,
    SCH: StateChangeHandler + 'static,
> User<P, M, Sc, H, SCH>
where
    M::Identity: Clone,
{
    /// Poll a `PendingJoin` group. Returns `true` while still waiting,
    /// `false` once joined or once the join attempt has been torn down
    /// after timing out.
    pub async fn check_pending_join(&self, group_name: &str) -> Result<bool, UserError> {
        let (state, expired) = match self
            .with_entry(group_name, |entry| {
                (
                    entry.state_machine.current_state(),
                    entry.state_machine.is_pending_join_expired(),
                )
            })
            .await
        {
            Some(v) => v,
            None => return Ok(false),
        };

        if state != GroupState::PendingJoin {
            return Ok(false);
        }

        if expired {
            info!(group = group_name, "pending join timed out");
            self.groups.write().await.remove(group_name);
            self.cleanup_consensus_scope(group_name).await?;
            let _ = self.handler.on_leave_group(group_name).await;
            return Ok(false);
        }

        Ok(true)
    }

    /// Poll tick for `Freezing`: drives Freezing → Selection once candidates
    /// are all in or the freeze window elapses, then finalises and dispatches.
    pub async fn poll_freeze_status(
        &self,
        group_name: &str,
    ) -> Result<FreezeTimeoutStatus, UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;

        let has_proposals = {
            let mut entry = entry_arc.write().await;

            let state = entry.state_machine.current_state();
            if state != GroupState::Freezing {
                return Ok(FreezeTimeoutStatus::NotFreezing);
            }

            // Early selection: skip remaining freeze time if all expected
            // stewards have submitted candidates.
            let all_candidates_in = entry
                .group
                .steward_list()
                .is_some_and(|list| entry.group.freeze_candidate_count() >= list.len());

            if !all_candidates_in && !entry.state_machine.is_freeze_timed_out() {
                return Ok(FreezeTimeoutStatus::StillFreezing);
            }

            entry.state_machine.start_selection();
            entry.group.approved_proposals_count() > 0
        };

        self.state_handler
            .on_state_changed(group_name, GroupState::Selection)
            .await;

        let (finalize_result, downward_cross) = {
            let mut entry = entry_arc.write().await;
            let allow_subset = entry.group.allow_subset_candidates();
            let result = if entry.group.mls().is_some() {
                match finalize_freeze_round(&mut entry.group, allow_subset, &self.app_id) {
                    Ok(result) => result,
                    Err(e) => {
                        error!(group = group_name, error = %e, "freeze finalize failed");
                        FreezeFinalizeResult::default()
                    }
                }
            } else {
                FreezeFinalizeResult::default()
            };
            entry.archive_committed_batch(result.committed_batch.clone());
            // Apply locally-observed score events before releasing the
            // entry lock. These come from dropped candidates in the
            // phase-3 loop (RFC §Peer Scoring: direct local observation,
            // no ECP needed). A downward threshold cross schedules a
            // removal-init pass below, after the lock drops.
            let cross = if !result.score_ops.is_empty() {
                let events = entry.scoring.apply_ops(&result.score_ops);
                has_downward_cross(&events)
            } else {
                false
            };
            (result, cross)
        };

        // Lock split is intentional: `check_and_initiate_score_removals`
        // re-acquires the entry write lock and calls `initiate_proposal`
        // which `.await`s on the consensus service. Holding the entry
        // lock across that await would block other operations on this
        // group, so we drop the lock above before chaining.
        if downward_cross && let Err(e) = self.check_and_initiate_score_removals(group_name).await {
            error!(group = group_name, error = %e, "score-removal check failed (freeze finalize)");
        }

        match finalize_result.outcome {
            FreezeOutcome::Applied { result, outbound } => {
                // Welcomes are deferred to here so joiners can't advance
                // epoch ahead of the steward.
                let has_welcome = outbound
                    .as_ref()
                    .is_some_and(|p| p.subtopic == WELCOME_SUBTOPIC);
                if let Some(packet) = outbound {
                    if let Err(e) = self.handler.on_outbound(group_name, packet).await {
                        error!(group = group_name, error = %e, "deferred welcome send failed");
                    }
                }

                // GroupSync carries the steward list + timing + scores to
                // new joiners; send it only after the welcome they'll use
                // to catch up.
                if has_welcome {
                    if let Err(e) = self.send_group_sync(group_name).await {
                        error!(group = group_name, error = %e, "group sync send failed");
                    }
                }

                if let Err(e) = self.dispatch_inbound_result(group_name, *result).await {
                    error!(group = group_name, error = %e, "finalize result dispatch failed");
                }
                return Ok(FreezeTimeoutStatus::Applied);
            }
            FreezeOutcome::NoCandidate => {
                // `accuse_target` is `Some` only when we had approved proposals
                // go unanswered *and* can attribute the miss to a live steward
                // other than ourselves. Self-penalties are skipped — the
                // node that failed to commit observes its own state directly
                // and doesn't need to record a ScoreOp against itself.
                let (next_state, downward_cross) = {
                    let mut entry = entry_arc.write().await;

                    if has_proposals {
                        // Approved batch (and in-flight votes) survive so
                        // the recovered steward commits the same proposals
                        // once the next election lands.
                        entry.state_machine.clear_proposal_timer();
                        entry.state_machine.start_reelection();

                        // Local observation → direct peer-score penalty,
                        // no ECP round-trip. Each honest member records
                        // the same event independently; threshold-crossing
                        // removal still goes through SCORE_BELOW_THRESHOLD
                        // consensus in steward.rs.
                        let accuse_target = match entry.group.mls() {
                            Some(mls) => {
                                let violation_epoch = mls.current_epoch()?;
                                let self_identity = self.identity().identity_bytes();
                                let members = group_members(&entry.group)?;
                                entry
                                    .group
                                    .live_epoch_steward(violation_epoch, &members)
                                    .filter(|id| !id.is_empty() && *id != self_identity)
                                    .map(|id| id.to_vec())
                            }
                            None => None,
                        };
                        let cross = if let Some(steward_id) = accuse_target {
                            let events = entry.scoring.apply_op(&ScoreOp {
                                member_id: steward_id,
                                event: ScoreEvent::CensorshipInactivity,
                            });
                            has_downward_cross(&events)
                        } else {
                            false
                        };

                        (GroupState::Reelection, cross)
                    } else {
                        entry.group.clear_freeze_round();
                        entry.state_machine.start_working();
                        (GroupState::Working, false)
                    }
                };

                if downward_cross
                    && let Err(e) = self.check_and_initiate_score_removals(group_name).await
                {
                    error!(group = group_name, error = %e, "score-removal check failed (freeze timeout)");
                }

                let entered_reelection = next_state == GroupState::Reelection;
                self.state_handler
                    .on_state_changed(group_name, next_state)
                    .await;

                // Layer 2 recovery: regenerate the steward list. Only the
                // responsible proposer's call actually submits.
                if entered_reelection
                    && let Err(e) = self
                        .try_initiate_steward_election(group_name, true, None)
                        .await
                {
                    info!(group = group_name, error = %e, "recovery election deferred");
                }
            }
        }

        Ok(FreezeTimeoutStatus::TimedOut { has_proposals })
    }

    /// Drives Working → Freezing when the steward has sat on approved
    /// proposals for more than `epoch_duration`. Any member polls — not
    /// just the steward — so a fault gets picked up group-wide.
    pub async fn check_member_freeze(&self, group_name: &str) -> Result<bool, UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let mut entry = entry_arc.write().await;

        let state = entry.state_machine.current_state();
        if state == GroupState::PendingJoin || state == GroupState::Leaving {
            return Ok(false);
        }

        let proposal_count = entry.group.approved_proposals_count();
        // Hold the freeze while an election is in flight — committing on
        // the known-stale list would just produce a NoCandidate.
        if entry.group.has_election_in_flight() {
            return Ok(false);
        }
        // Recovery uses the shorter retry inactivity window so we don't
        // burn another full epoch waiting for a steward to commit.
        let in_recovery = entry.group.is_in_recovery_mode() || entry.group.reelection_round() > 0;
        let inactivity = if in_recovery {
            entry.state_machine.retry_inactivity_duration()
        } else {
            entry.state_machine.epoch_duration()
        };
        let entered_freezing = entry
            .state_machine
            .check_steward_inactivity(proposal_count, inactivity);
        if entered_freezing {
            let epoch = entry.group.expect_mls()?.current_epoch()?;
            entry.group.ensure_freeze_round(epoch);

            // Stewards build their own candidate under the same lock.
            // Candidate-build failure must not block the freeze transition —
            // peers' candidates still get processed.
            let outbound = if entry.group.is_steward() {
                match create_commit_candidate(&mut entry.group, &self.app_id) {
                    Ok(packets) => packets,
                    Err(e) => {
                        error!(
                            group = group_name,
                            error = %e,
                            "commit candidate build failed"
                        );
                        None
                    }
                }
            } else {
                None
            };

            let new_state = entry.state_machine.current_state();
            info!(
                group = group_name,
                state = %new_state,
                approved = proposal_count,
                "steward inactivity transition"
            );

            // Drop lock before any async calls.
            drop(entry);
            self.state_handler
                .on_state_changed(group_name, new_state)
                .await;
            if let Some(message) = outbound {
                self.handler.on_outbound(group_name, message).await?;
            }
        }

        Ok(entered_freezing)
    }
}
