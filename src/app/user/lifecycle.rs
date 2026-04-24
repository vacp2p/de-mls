//! Create and leave operations for a group.

use tracing::info;

use crate::{
    app::{
        GroupConfig, GroupState, GroupStateMachine, ProposalParams, StateChangeHandler, User,
        UserError, submit_self_leave_proposal, user::GroupEntry,
    },
    core::{DeMlsProvider, GroupEventHandler, build_message, create_group, prepare_to_join},
    mls_crypto::parse_wallet_to_bytes,
    protos::de_mls::messages::v1::GroupUpdateRequest,
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

        let max_reelection_retries = config.max_reelection_retries;
        let (mut group, state_machine) = if is_creation {
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
        group.set_max_reelection_retries(max_reelection_retries);

        let initial_state = state_machine.current_state();
        if initial_state == GroupState::PendingJoin {
            info!(
                group = group_name,
                timeout_s = state_machine.epoch_duration().as_secs() * 3,
                "pending join, awaiting welcome"
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

    /// Submit `RemoveMember(self)` as an `expected_voters = 1` proposal with
    /// the leaver's YES bundled, so the session resolves on submit. We stay
    /// active until the next steward commit merges the removal; on that
    /// commit `ProcessResult::LeaveGroup` fires. A failed commit leaves us
    /// a member and a later one picks the leave up.
    /// `PendingJoin` short-circuits to local teardown.
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!(group = group_name, "leaving group");

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

        // Idempotent: a second click after a successful submit finds the
        // approved entry and short-circuits.
        {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            if entry.group.is_pending_self_leave(&self_identity) {
                info!(
                    group = group_name,
                    "self-leave already in flight, ignoring duplicate"
                );
                return Ok(());
            }
        }

        let request = {
            use crate::protos::de_mls::messages::v1::{RemoveMember, group_update_request};
            GroupUpdateRequest {
                payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                    identity: self_identity.clone(),
                })),
            }
        };

        // Register ownership BEFORE the session opens — the bundled YES
        // fires `ConsensusReached` on submit, and `apply_consensus_result`
        // needs `is_owner_of_proposal` to be true by then.
        let proposal_id = crate::core::auto_approved_leave_proposal_id(&self_identity);
        {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            entry.group.store_voting_proposal(proposal_id, request);
        }

        let config = &self.default_group_config;
        let submitted = submit_self_leave_proposal::<P, _>(
            group_name,
            &self_identity,
            &self.consensus_service,
            self.eth_signer.clone(),
            ProposalParams {
                expected_voters: 1,
                proposal_expiration: config.proposal_expiration,
                consensus_timeout: config.consensus_timeout,
                liveness_criteria_yes: true,
            },
        )
        .await?;

        // Dedup (`ProposalAlreadyExist`) — another submit is already driving
        // this proposal_id. Our voting entry resolves on that session.
        let Some((_proposal_id, app_msg)) = submitted else {
            return Ok(());
        };

        let packet = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            build_message(&entry.group, &self.mls_service, &app_msg, &self.app_id)?
        };
        self.handler.on_outbound(group_name, packet).await?;

        Ok(())
    }
}
