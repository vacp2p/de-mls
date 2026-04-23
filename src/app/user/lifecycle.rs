//! Create and leave operations for a group.

use tracing::info;

use crate::{
    app::{
        GroupConfig, GroupState, GroupStateMachine, StateChangeHandler, User, UserError,
        user::GroupEntry,
    },
    core::{DeMlsProvider, GroupEventHandler, build_message, create_group, prepare_to_join},
    mls_crypto::parse_wallet_to_bytes,
    protos::de_mls::messages::v1::{AppMessage, LeaveRequest},
};

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Create (`is_creation = true`) or join (`false`) a group using the
    /// user's default config.
    pub async fn create_group(
        &mut self,
        group_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        self.create_group_with_config(group_name, is_creation, self.default_group_config.clone())
            .await
    }

    /// Like [`Self::create_group`] but with a per-group config override.
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
            let group = create_group(group_name, &self.mls_service, config.protocol.clone())?;
            let state_machine = GroupStateMachine::new_as_member_with_config(config);
            (group, state_machine)
        } else {
            let group = prepare_to_join(
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

        // Joiners start tracked at `JoinedGroup` time (once members are known).
        if is_creation {
            self.scoring()
                .add_member(group_name, &self.mls_service.wallet_bytes());
        }

        self.state_handler
            .on_state_changed(group_name, initial_state)
            .await;

        Ok(())
    }

    /// Broadcast a signed `LeaveRequest` and locally accept it (deterministic
    /// proposal id → approved queue, no consensus round). The next live
    /// steward's commit includes the self-remove.
    ///
    /// The leaver stays fully active until that commit actually merges; at
    /// that point `ProcessResult::LeaveGroup` fires and the UI closes the
    /// view. This avoids a half-leave state — if the commit fails we just
    /// stay a member and a later commit picks the leave up.
    ///
    /// `PendingJoin` short-circuits: nothing to leave, just tear down local state.
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!("[leave_group]: Leaving group {group_name}");

        let is_pending_join = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .map(|entry| entry.state_machine.current_state() == GroupState::PendingJoin)
                .ok_or(UserError::GroupNotFound)?
        };
        if is_pending_join {
            self.groups.write().await.remove(group_name);
            self.cleanup_consensus_scope(group_name).await?;
            self.handler.on_leave_group(group_name).await?;
            return Ok(());
        }

        let self_identity = parse_wallet_to_bytes(&self.identity_string())?;

        // Idempotent: second click after a broadcast is a no-op.
        {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            if entry.group.is_pending_self_leave(&self_identity) {
                info!(
                    "[leave_group]: self-leave already in flight for {group_name}, \
                     ignoring duplicate"
                );
                return Ok(());
            }
        }

        // Build + MLS-encrypt + broadcast on the app subtopic.
        let packet = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let app_msg: AppMessage = LeaveRequest {
                identity: self_identity.clone(),
            }
            .into();
            build_message(&entry.group, &self.mls_service, &app_msg, &self.app_id)?
        };
        self.handler.on_outbound(group_name, packet).await?;

        // The inactivity timer self-starts on the next `check_steward_inactivity` poll.
        {
            let mut groups = self.groups.write().await;
            if let Some(entry) = groups.get_mut(group_name) {
                entry.group.accept_auto_approved_leave(self_identity);
            }
        }

        Ok(())
    }
}
