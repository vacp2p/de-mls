//! Timer polls for pending-join expiry, freeze timeout, and steward inactivity.

use tracing::{error, info};

use crate::{
    app::{FreezeTimeoutStatus, GroupState, StateChangeHandler, User, UserError},
    core::{
        DeMlsProvider, FreezeFinalizeResult, FreezeOutcome, GroupEventHandler, ScoreEvent, ScoreOp,
        create_commit_candidate, finalize_freeze_round, group_members,
    },
};

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Poll a `PendingJoin` group. Returns `true` while still waiting,
    /// `false` once joined or once the join attempt has been torn down
    /// after timing out.
    pub async fn check_pending_join(&self, group_name: &str) -> Result<bool, UserError> {
        let (state, expired) = {
            let groups = self.groups.read().await;
            match groups.get(group_name) {
                Some(entry) => (
                    entry.state_machine.current_state(),
                    entry.state_machine.is_pending_join_expired(),
                ),
                None => return Ok(false),
            }
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
        let has_proposals = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

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

        let finalize_result = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            let allow_subset = entry.group.allow_subset_candidates();
            match finalize_freeze_round(
                &mut entry.group,
                &self.mls_service,
                allow_subset,
                &self.app_id,
            ) {
                Ok(result) => result,
                Err(e) => {
                    error!(group = group_name, error = %e, "freeze finalize failed");
                    FreezeFinalizeResult::default()
                }
            }
        };

        // Apply locally-observed score events before dispatching the
        // outcome. These come from dropped candidates in the phase-3 loop
        // (RFC §Peer Scoring: direct local observation, no ECP needed).
        if !finalize_result.score_ops.is_empty() {
            self.scoring()
                .apply_ops(group_name, &finalize_result.score_ops);
        }

        match finalize_result.outcome {
            FreezeOutcome::Applied { result, outbound } => {
                // Welcomes are deferred to here so joiners can't advance
                // epoch ahead of the steward.
                let has_welcome = outbound
                    .as_ref()
                    .is_some_and(|p| p.subtopic == crate::ds::WELCOME_SUBTOPIC);
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
                let (next_state, accuse_target) = {
                    let mut groups = self.groups.write().await;
                    let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

                    if has_proposals {
                        entry.group.reject_all_approved_proposals();
                        entry.group.reject_all_voting_proposals();
                        entry.state_machine.clear_proposal_timer();
                        entry.state_machine.start_reelection();

                        let violation_epoch = self.mls_service.current_epoch(group_name)?;
                        let self_identity = self.mls_service.wallet_bytes();
                        let members = group_members(&entry.group, &self.mls_service)?;
                        let target = entry
                            .group
                            .live_epoch_steward(violation_epoch, &members)
                            .filter(|id| !id.is_empty() && *id != self_identity.as_slice())
                            .map(|id| id.to_vec());

                        (GroupState::Reelection, target)
                    } else {
                        entry.group.clear_freeze_round();
                        entry.state_machine.start_working();
                        (GroupState::Working, None)
                    }
                };

                self.state_handler
                    .on_state_changed(group_name, next_state)
                    .await;

                if let Some(steward_id) = accuse_target {
                    // Local observation → direct peer-score penalty, no ECP
                    // round-trip. Each honest member records the same event
                    // independently; threshold-crossing removal still goes
                    // through SCORE_BELOW_THRESHOLD consensus in steward.rs.
                    self.scoring().apply_op(
                        group_name,
                        &ScoreOp {
                            member_id: steward_id,
                            event: ScoreEvent::CensorshipInactivity,
                        },
                    );
                }
            }
        }

        Ok(FreezeTimeoutStatus::TimedOut { has_proposals })
    }

    /// Drives Working → Freezing when the steward has sat on approved
    /// proposals for more than `epoch_duration`. Any member polls — not
    /// just the steward — so a fault gets picked up group-wide.
    pub async fn check_member_freeze(&self, group_name: &str) -> Result<bool, UserError> {
        let mut groups = self.groups.write().await;
        let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

        let state = entry.state_machine.current_state();
        if state == GroupState::PendingJoin || state == GroupState::Leaving {
            return Ok(false);
        }

        let proposal_count = entry.group.approved_proposals_count();
        let entered_freezing = entry.state_machine.check_steward_inactivity(proposal_count);
        if entered_freezing {
            let epoch = self.mls_service.current_epoch(group_name)?;
            entry.group.ensure_freeze_round(epoch);

            // Stewards build their own candidate under the same lock.
            // Candidate-build failure must not block the freeze transition —
            // peers' candidates still get processed.
            let outbound = if entry.group.is_steward() {
                match create_commit_candidate(&mut entry.group, &self.mls_service, &self.app_id) {
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
            drop(groups);
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
