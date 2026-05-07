//! Create and leave operations for a group.

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;

use crate::{
    app::{
        GroupConfig, GroupState, GroupStateMachine, ProposalParams, StateChangeHandler, User,
        UserError, submit_self_leave_proposal, user::GroupEntry,
    },
    core::{
        DeMlsProvider, Group, GroupEventHandler, PeerScoringPlugin, StewardListPlugin,
        auto_approved_leave_proposal_id,
    },
    identity::Identity,
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{GroupUpdateRequest, RemoveMember, group_update_request},
};

impl<
    P: DeMlsProvider,
    M: MlsService,
    Sc: PeerScoringPlugin,
    St: StewardListPlugin,
    I: Identity,
    H: GroupEventHandler + 'static,
    SCH: StateChangeHandler + 'static,
> User<P, M, Sc, St, I, H, SCH>
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

        let max_reelection_attempts = config.max_reelection_attempts;
        let liveness_criteria_yes = config.liveness_criteria_yes;
        let pending_update_max_epochs = config.pending_update_max_epochs;
        let self_identity_bytes = self.identity().identity_bytes().to_vec();
        let (mut group, mls_opt, state_machine) = if is_creation {
            let mls = (self.mls_creator_factory)(group_name.to_string())?;
            let group = Group::create_group(group_name, self_identity_bytes.clone());
            let state_machine = GroupStateMachine::new_as_member_with_config(config.clone());
            (group, Some(mls), state_machine)
        } else {
            let group = Group::prepare_to_join(group_name, self_identity_bytes.clone());
            let state_machine = GroupStateMachine::new_as_pending_join_with_config(config.clone());
            (group, None, state_machine)
        };
        group.set_liveness_criteria_yes(liveness_criteria_yes);
        group.set_pending_update_max_epochs(pending_update_max_epochs);

        let mut steward = self.make_steward_plugin(group_name, &config.protocol);
        steward.set_max_retries(max_reelection_attempts);
        // Creator path: bootstrap the list with self as sole steward at
        // epoch 0. Joiner path leaves the plug-in empty until `GroupSync`.
        if is_creation {
            let _events =
                steward.install_list(0, std::slice::from_ref(&self_identity_bytes), 1, 0)?;
        }

        let mut scoring = self.make_scoring_service();
        // Joiners start tracked at `JoinedGroup` time (once members are known).
        if is_creation {
            // Creator is self at `default_score`; under standard config
            // (`default > threshold`) no cross event fires, so we drop
            // the return value here.
            let _events = scoring.add_member(&self_identity_bytes);
        }

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
            Arc::new(RwLock::new(GroupEntry::new(
                group,
                mls_opt,
                state_machine,
                scoring,
                steward,
            ))),
        );
        drop(groups);

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

        let is_pending_join = self
            .with_entry(group_name, |entry| {
                entry.state_machine.current_state() == GroupState::PendingJoin
            })
            .await
            .ok_or(UserError::GroupNotFound)?;
        if is_pending_join {
            self.groups.write().await.remove(group_name);
            self.cleanup_consensus_scope(group_name).await?;
            self.handler.on_leave_group(group_name).await?;
            return Ok(());
        }

        let self_identity = self.identity().identity_bytes().to_vec();

        // Idempotent: a second click after a successful submit finds the
        // approved entry and short-circuits.
        let already_pending = self
            .with_entry(group_name, |entry| {
                entry.group.is_pending_self_leave(&self_identity)
            })
            .await
            .ok_or(UserError::GroupNotFound)?;
        if already_pending {
            info!(
                group = group_name,
                "self-leave already in flight, ignoring duplicate"
            );
            return Ok(());
        }

        let request = GroupUpdateRequest {
            payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                identity: self_identity.clone(),
            })),
        };

        // Register ownership BEFORE the session opens — the bundled YES
        // fires `ConsensusReached` on submit, and `apply_consensus_result`
        // needs `is_owner_of_proposal` to be true by then.
        let proposal_id = auto_approved_leave_proposal_id(&self_identity);
        let (proposal_expiration, consensus_timeout) = {
            let entry_arc = self
                .lookup_entry(group_name)
                .await
                .ok_or(UserError::GroupNotFound)?;
            let mut entry = entry_arc.write().await;
            entry.group.store_voting_proposal(proposal_id, request);
            (
                entry.state_machine.proposal_expiration(),
                entry.state_machine.consensus_timeout(),
            )
        };

        let submitted = submit_self_leave_proposal::<P, _>(
            group_name,
            &self_identity,
            &self.consensus_service,
            self.eth_signer.clone(),
            ProposalParams {
                expected_voters: 1,
                proposal_expiration,
                consensus_timeout,
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
            let entry_arc = self
                .lookup_entry(group_name)
                .await
                .ok_or(UserError::GroupNotFound)?;
            let entry = entry_arc.read().await;
            entry.expect_mls()?.build_message(&app_msg, &self.app_id)?
        };
        self.handler.on_outbound(group_name, packet).await?;

        Ok(())
    }
}
