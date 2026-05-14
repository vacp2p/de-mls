//! Create and leave operations for a conversation.

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;

use crate::{
    app::{
        ConversationState, PhaseTimer, ProposalParams, SessionRunner, User, UserError,
        submit_self_leave_proposal,
    },
    core::{
        ConsensusPlugin, Conversation, ConversationConfig, ConversationLifecycle,
        ConversationPluginsFactory, ConversationStateMachine, PeerScoringPlugin, SessionEvent,
        StewardListPlugin, self_leave_proposal_id,
    },
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, RemoveMember, conversation_update_request,
    },
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    pub async fn start_conversation(
        &mut self,
        conversation_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        self.create_conversation_with_config(
            conversation_name,
            is_creation,
            self.default_conversation_config.clone(),
        )
        .await
    }

    /// Like [`Self::start_conversation`] but with a per-conversation config override.
    pub async fn create_conversation_with_config(
        &mut self,
        conversation_name: &str,
        is_creation: bool,
        config: ConversationConfig,
    ) -> Result<(), UserError> {
        let mut conversations = self.conversations.write().await;
        if conversations.contains_key(conversation_name) {
            return Err(UserError::ConversationAlreadyExists);
        }

        let self_identity_bytes = self.self_identity().to_vec();
        let (conversation, mls_opt, state_machine, phase_timer) = if is_creation {
            let mls = self
                .plugin_factory
                .create_mls(conversation_name.to_string())?;
            let conversation = Conversation::new(conversation_name);
            let state_machine = ConversationStateMachine::new_as_member();
            (conversation, Some(mls), state_machine, PhaseTimer::new())
        } else {
            let conversation = Conversation::new(conversation_name);
            let state_machine = ConversationStateMachine::new_as_pending_join();
            // Anchor the timer at "now" so `is_pending_join_expired` can
            // detect the 3× commit-inactivity timeout.
            let mut phase_timer = PhaseTimer::new();
            phase_timer.start();
            (conversation, None, state_machine, phase_timer)
        };

        let mut steward_list = self.plugin_factory.make_steward_list(
            conversation_name.as_bytes(),
            self.default_steward_list_config.clone(),
        );
        steward_list.set_max_retries(config.max_reelection_attempts);
        // Creator path: bootstrap the list with self as sole steward at
        // epoch 0. Joiner path leaves the plug-in empty until `ConversationSync`.
        if is_creation {
            let _events =
                steward_list.install_list(0, std::slice::from_ref(&self_identity_bytes), 1, 0)?;
        }

        let mut scoring = self
            .plugin_factory
            .make_scoring(&self.default_scoring_config);
        // Joiners get tracked at `JoinedConversation` time, once members are known.
        if is_creation {
            // Creator is self at `default_score`; under standard config
            // (`default > threshold`) no cross event fires, so we drop
            // the return value here.
            let _events = scoring.add_member(&self_identity_bytes);
        }

        let initial_state = state_machine.current_state();
        if initial_state == ConversationState::PendingJoin {
            info!(
                conversation = conversation_name,
                timeout_s = config.commit_inactivity_duration.as_secs() * 3,
                "pending join, awaiting welcome"
            );
        }
        let consensus = self.build_consensus_service();
        conversations.insert(
            conversation_name.to_string(),
            Arc::new(RwLock::new(SessionRunner::new(
                conversation_name.to_string(),
                conversation,
                mls_opt,
                state_machine,
                phase_timer,
                config,
                scoring,
                steward_list,
                consensus,
                Arc::clone(&self.self_identity),
                Arc::clone(&self.app_id),
            ))),
        );
        drop(conversations);

        // Emit on the lifecycle channel first so integrators can subscribe
        // to the new session's `SessionEvent` stream before any session
        // events are fired.
        let _ = self.lifecycle.send(ConversationLifecycle::Created(
            conversation_name.to_string(),
        ));
        self.emit(conversation_name, SessionEvent::PhaseChange(initial_state))
            .await;

        Ok(())
    }

    /// Submit `RemoveMember(self)` as an `expected_voters = 1` proposal with
    /// the leaver's YES bundled, so the session resolves on submit. We stay
    /// active until the next steward commit merges the removal; on that
    /// commit `ProcessResult::LeaveConversation` fires. A failed commit leaves us
    /// a member and a later one picks the leave up.
    /// `PendingJoin` short-circuits to local teardown.
    pub async fn leave_conversation(&mut self, conversation_name: &str) -> Result<(), UserError> {
        info!(conversation = conversation_name, "leaving conversation");

        let is_pending_join = self
            .with_entry(conversation_name, |entry| {
                entry.handle.current_state() == ConversationState::PendingJoin
            })
            .await
            .ok_or(UserError::ConversationNotFound)?;
        if is_pending_join {
            self.emit(conversation_name, SessionEvent::Leaving).await;
            self.conversations.write().await.remove(conversation_name);
            self.cleanup_consensus_scope(conversation_name).await?;
            let _ = self.lifecycle.send(ConversationLifecycle::Removed(
                conversation_name.to_string(),
            ));
            return Ok(());
        }

        let self_identity = self.self_identity().to_vec();

        // Idempotent: a second click after a successful submit finds the
        // approved entry and short-circuits.
        let already_pending = self
            .with_entry(conversation_name, |entry| {
                entry
                    .handle
                    .conversation
                    .is_pending_self_leave(&self_identity)
            })
            .await
            .ok_or(UserError::ConversationNotFound)?;
        if already_pending {
            info!(
                conversation = conversation_name,
                "self-leave already in flight, ignoring duplicate"
            );
            return Ok(());
        }

        let request = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::RemoveMember(
                RemoveMember {
                    identity: self_identity.clone(),
                },
            )),
        };

        // Register ownership BEFORE the session opens — the bundled YES
        // fires `ConsensusReached` on submit, and `apply_consensus_result`
        // needs `is_owner_of_proposal` to be true by then.
        let proposal_id = self_leave_proposal_id(&self_identity);
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let submitted = {
            let mut entry = entry_arc.write().await;
            entry
                .handle
                .conversation
                .store_voting_proposal(proposal_id, request);
            let proposal_expiration = entry.handle.config.proposal_expiration;
            let consensus_timeout = entry.handle.config.consensus_timeout;
            submit_self_leave_proposal::<P>(
                conversation_name,
                &self_identity,
                &entry.consensus,
                ProposalParams {
                    expected_voters: 1,
                    proposal_expiration,
                    consensus_timeout,
                    liveness_criteria_yes: true,
                },
            )
            .await?
        };

        // Dedup (`ProposalAlreadyExist`) — another submit is already driving
        // this proposal_id. Our voting entry resolves on that session.
        let Some((_proposal_id, app_msg)) = submitted else {
            return Ok(());
        };

        let packet = {
            let entry_arc = self
                .lookup_entry(conversation_name)
                .await
                .ok_or(UserError::ConversationNotFound)?;
            let entry = entry_arc.read().await;
            entry
                .handle
                .expect_mls()?
                .build_message(&app_msg, &self.app_id)?
        };
        self.send_outbound(packet).await?;

        Ok(())
    }
}
