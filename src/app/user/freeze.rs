//! Freeze/commit lifecycle operations.

use super::*;

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Check if still in pending join state.
    ///
    /// Returns `true` if still waiting, `false` if joined or timed out.
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
            info!(
                "[check_pending_join]: Join timed out for group {group_name} \
                 (time-based fallback)"
            );
            self.groups.write().await.remove(group_name);
            self.cleanup_consensus_scope(group_name).await?;
            let _ = self.handler.on_leave_group(group_name).await;
            return Ok(false);
        }

        Ok(true)
    }

    /// Check if the freeze phase timed out.
    ///
    /// Call this periodically while a group is in `Freezing` state.
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
            match core::finalize_freeze_round(
                &mut entry.group,
                &self.mls_service,
                allow_subset,
                &self.app_id,
            ) {
                Ok(result) => result,
                Err(e) => {
                    error!("[poll_freeze_status] finalize_freeze_round failed: {e}");
                    FreezeFinalizeResult::NoCandidate
                }
            }
        };

        match finalize_result {
            FreezeFinalizeResult::Applied { result, outbound } => {
                // Send deferred welcome packets now that commit is merged
                let has_welcome = outbound
                    .iter()
                    .any(|p| p.subtopic == crate::ds::WELCOME_SUBTOPIC);
                for packet in outbound {
                    if let Err(e) = self.handler.on_outbound(group_name, packet).await {
                        error!("[poll_freeze_status] Failed to send deferred welcome: {e}");
                    }
                }

                // Send group sync to new joiners after welcome packets
                if has_welcome {
                    if let Err(e) = self.send_group_sync(group_name).await {
                        error!("[poll_freeze_status] Failed to send group sync: {e}");
                    }
                }

                // Dispatch result first -- this transitions state to Working
                if let Err(e) = self.dispatch_inbound_result(group_name, result).await {
                    error!("[poll_freeze_status] Failed to dispatch finalize result: {e}");
                }
                return Ok(FreezeTimeoutStatus::Applied);
            }
            FreezeFinalizeResult::NoCandidate => {
                let (next_state, should_accuse, violation_epoch, steward_id) = {
                    let mut groups = self.groups.write().await;
                    let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

                    if has_proposals {
                        entry.group.reject_all_approved_proposals();
                        entry.group.reject_all_voting_proposals();
                        entry.state_machine.clear_proposal_timer();
                        entry.state_machine.start_reelection();
                        let violation_epoch =
                            self.mls_service.current_epoch(group_name).unwrap_or(0);
                        // Accuse the live epoch steward — the member who was
                        // actually responsible for committing given current
                        // membership. If the nominal steward was removed
                        // earlier this epoch, their identity is no longer in
                        // the group and accusing them would file an ECP
                        // against a ghost.
                        let members = core::group_members(&entry.group, &self.mls_service)
                            .unwrap_or_default();
                        let steward_id = entry
                            .group
                            .live_epoch_steward(violation_epoch, &members)
                            .filter(|id| !id.is_empty())
                            .map(|id| id.to_vec())
                            .unwrap_or_default();
                        (
                            GroupState::Reelection,
                            !steward_id.is_empty(),
                            violation_epoch,
                            steward_id,
                        )
                    } else {
                        entry.group.clear_freeze_round();

                        entry.state_machine.start_working();
                        (GroupState::Working, false, 0, Vec::new())
                    }
                };

                self.state_handler
                    .on_state_changed(group_name, next_state.clone())
                    .await;

                if should_accuse {
                    let request =
                        match ViolationEvidence::censorship_inactivity(steward_id, violation_epoch)
                            .with_creator(self.mls_service.wallet_bytes())
                            .into_update_request()
                        {
                            Ok(r) => r,
                            Err(e) => {
                                error!("[poll_freeze_status] Failed to build ECP: {e}");
                                return Ok(FreezeTimeoutStatus::TimedOut { has_proposals });
                            }
                        };
                    if let Err(e) = self
                        .initiate_proposal(group_name.to_string(), request)
                        .await
                    {
                        error!("[poll_freeze_status] Failed to start emergency criteria vote: {e}");
                    }
                }
            }
        }

        Ok(FreezeTimeoutStatus::TimedOut { has_proposals })
    }

    /// Check for steward inactivity and transition to Freezing if needed.
    ///
    /// If approved proposals exist and the epoch steward hasn't committed
    /// within `epoch_duration`, transition to Freezing so the freeze timeout
    /// can detect steward fault. Any member (steward or not) can detect this.
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

            // If steward, create commit candidate while we still hold the lock.
            // Candidate creation failure must not block the freeze transition —
            // this node can still process other stewards' candidates.
            let outbound = if entry.group.is_steward() {
                match create_commit_candidate(&mut entry.group, &self.mls_service, &self.app_id) {
                    Ok(packets) => packets,
                    Err(e) => {
                        error!(
                            "[check_member_freeze] Failed to create commit candidate \
                             for group {group_name}: {e}"
                        );
                        vec![]
                    }
                }
            } else {
                vec![]
            };

            let new_state = entry.state_machine.current_state();
            info!(
                "[check_member_freeze]: Steward inactivity → {} for group {group_name} \
                 ({proposal_count} approved proposals)",
                new_state
            );

            // Drop lock before any async calls.
            drop(groups);
            self.state_handler
                .on_state_changed(group_name, new_state)
                .await;
            for message in outbound {
                self.handler.on_outbound(group_name, message).await?;
            }
        }

        Ok(entered_freezing)
    }
}
