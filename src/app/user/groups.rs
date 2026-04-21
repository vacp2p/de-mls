//! Group CRUD and query operations.

use super::*;

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Create or join a group with the user's default config.
    pub async fn create_group(
        &mut self,
        group_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        self.create_group_with_config(group_name, is_creation, self.default_group_config.clone())
            .await
    }

    /// Create or join a group with custom config.
    pub async fn create_group_with_config(
        &mut self,
        group_name: &str,
        is_creation: bool,
        config: GroupConfig,
    ) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        if groups.contains_key(group_name) {
            return Err(UserError::GroupAlreadyExists);
        }

        let (group, state_machine) = if is_creation {
            let group = core::create_group(group_name, &self.mls_service, config.protocol.clone())?;
            let state_machine = GroupStateMachine::new_as_member_with_config(config);
            (group, state_machine)
        } else {
            let group = core::prepare_to_join(
                group_name,
                self.mls_service.wallet_bytes(),
                config.protocol.clone(),
            );
            let state_machine = GroupStateMachine::new_as_pending_join_with_config(config);
            (group, state_machine)
        };

        let initial_state = state_machine.current_state();
        if initial_state == GroupState::PendingJoin {
            info!(
                "[create_group] Group {group_name}: PendingJoin, waiting for welcome \
                 (timeout={}s)",
                state_machine.epoch_duration().as_secs() * 3,
            );
        }
        groups.insert(
            group_name.to_string(),
            GroupEntry {
                group,
                state_machine,
            },
        );
        drop(groups);

        // Register creator in peer scoring (only if actually creating, not joining)
        if is_creation {
            self.scoring()
                .add_member(group_name, &self.mls_service.wallet_bytes());
        }

        self.state_handler
            .on_state_changed(group_name, initial_state)
            .await;

        Ok(())
    }

    /// Leave a group.
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!("[leave_group]: Leaving group {group_name}");

        let (old_state, new_state) = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            let old_state = entry.state_machine.current_state();
            match old_state {
                GroupState::PendingJoin => {
                    groups.remove(group_name);
                    drop(groups);
                    self.cleanup_consensus_scope(group_name).await?;
                    self.handler.on_leave_group(group_name).await?;
                    return Ok(());
                }
                GroupState::Reelection => {
                    return Err(UserError::GroupBlocked(old_state.to_string()));
                }
                GroupState::Leaving => return Err(UserError::AlreadyLeaving),
                _ => {
                    entry.state_machine.start_leaving();
                }
            }
            (old_state, entry.state_machine.current_state())
        };

        self.state_handler
            .on_state_changed(group_name, new_state.clone())
            .await;

        info!(
            "[leave_group]: Transitioning from {old_state} to Leaving, sending self-removal for group {group_name}"
        );

        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: parse_wallet_to_bytes(&self.identity_string())?,
            })),
        };

        // Buffer the self-removal on our own Group so that if this commit
        // round fails (self-remove MLS error, steward offline), the intent
        // survives and the next epoch steward can retry via
        // `process_buffered_updates`. Peers buffer independently when they
        // receive the proposal on the app subtopic (see `dispatch_inbound_result`).
        {
            let epoch = self.mls_service.current_epoch(group_name).unwrap_or(0);
            let mut groups = self.groups.write().await;
            if let Some(entry) = groups.get_mut(group_name) {
                entry.group.buffer_pending_update(request.clone(), epoch);
            }
        }

        self.initiate_proposal(group_name.to_string(), request)
            .await?;
        Ok(())
    }

    /// Get the state of a group.
    pub async fn get_group_state(&self, group_name: &str) -> Result<GroupState, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        Ok(entry.state_machine.current_state())
    }

    /// List all group names.
    pub async fn list_groups(&self) -> Vec<String> {
        let groups = self.groups.read().await;
        groups.keys().cloned().collect()
    }

    /// Get freeze round candidate count: (received, expected).
    ///
    /// `received` is the number of buffered commit candidates.
    /// `expected` is the steward list size (one candidate per steward).
    /// Returns `(0, 0)` if not in freeze or no steward list.
    pub async fn get_freeze_candidate_count(
        &self,
        group_name: &str,
    ) -> Result<(usize, usize), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let received = entry.group.freeze_candidate_count();
        let expected = entry.group.steward_list().map(|l| l.len()).unwrap_or(0);
        Ok((received, expected))
    }

    /// Check if the user is steward for a group.
    pub async fn is_steward_for_group(&self, group_name: &str) -> Result<bool, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        Ok(entry.group.is_steward())
    }

    /// Get the members of a group.
    pub async fn get_group_members(&self, group_name: &str) -> Result<Vec<String>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

        if !self.mls_service.has_group(entry.group.group_name()) {
            return Ok(Vec::new());
        }

        let members = core::group_members(&entry.group, &self.mls_service)?;
        Ok(members
            .into_iter()
            .map(|raw| format_wallet_address(raw.as_slice()).to_string())
            .collect())
    }

    /// Get member scores for a group from the peer scoring service.
    pub fn get_member_scores(&self, group_name: &str) -> Vec<(Vec<u8>, i64)> {
        self.scoring().all_members_with_scores(group_name)
    }

    /// Get the score for a specific member in a group.
    pub fn get_member_score(&self, group_name: &str, member_id: &[u8]) -> Option<i64> {
        self.scoring().score_for(group_name, member_id)
    }

    /// Get the steward role for each member in a group.
    pub async fn get_member_roles(
        &self,
        group_name: &str,
    ) -> Result<Vec<(Vec<u8>, MemberRole)>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let epoch = self.mls_service.current_epoch(group_name)?;
        let members = core::group_members(&entry.group, &self.mls_service)?;

        let list = entry.group.steward_list();
        // Use live rotation so removed stewards are skipped in the role display.
        // All members with the same steward list + MLS member set compute the
        // same live epoch/backup steward.
        let live_epoch = list.and_then(|l| l.live_epoch_steward(epoch, &members));
        let live_backup = list.and_then(|l| l.live_backup_steward(epoch, &members));
        let roles = members
            .iter()
            .cloned()
            .map(|id| {
                let role = match list {
                    Some(l) if !l.is_exhausted(epoch) => {
                        if live_epoch.is_some_and(|es| es == id) {
                            MemberRole::EpochSteward
                        } else if live_backup.is_some_and(|bs| bs == id) {
                            MemberRole::BackupSteward
                        } else if l.contains(&id) {
                            MemberRole::Steward
                        } else {
                            MemberRole::Member
                        }
                    }
                    Some(l) if l.contains(&id) => MemberRole::Steward,
                    _ => MemberRole::Member,
                };
                (id, role)
            })
            .collect();
        Ok(roles)
    }

    /// Get current epoch proposals for a group.
    pub async fn get_approved_proposal_for_current_epoch(
        &self,
        group_name: &str,
    ) -> Result<Vec<GroupUpdateRequest>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let approved_proposals = entry.group.approved_proposals();
        let display_proposals: Vec<GroupUpdateRequest> = approved_proposals.into_values().collect();
        Ok(display_proposals)
    }

    /// Get epoch history for a group.
    pub async fn get_epoch_history(
        &self,
        group_name: &str,
    ) -> Result<Vec<Vec<GroupUpdateRequest>>, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let history = entry.group.epoch_history();
        Ok(history
            .iter()
            .map(|batch| batch.values().cloned().collect())
            .collect())
    }
}
