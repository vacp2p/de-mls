//! Create and leave operations for a conversation.

use std::sync::{Arc, RwLock};

use tracing::info;

use crate::{
    app::{ConversationState, LockExt, PhaseTimer, SessionRunner, User, UserError},
    core::{
        ConsensusPlugin, Conversation, ConversationConfig, ConversationLifecycle,
        ConversationPluginsFactory, ConversationStateMachine, PeerScoringPlugin, SessionEvent,
        StewardListPlugin,
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
            self.plugins.default_conversation_config.clone(),
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
        if self
            .conversations
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .contains_key(conversation_name)
        {
            return Err(UserError::ConversationAlreadyExists);
        }

        let self_identity_bytes = self.self_identity().to_vec();
        let (conversation, mls_opt, state_machine, phase_timer) = if is_creation {
            let mls = self
                .plugins
                .conversation_plugins
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

        let mut steward_list = self.plugins.conversation_plugins.make_steward_list(
            conversation_name.as_bytes(),
            self.plugins.default_steward_list_config.clone(),
        );
        steward_list.set_max_retries(config.max_reelection_attempts);
        // Creator path: bootstrap the list with self as sole steward at
        // epoch 0. Joiner path leaves the plug-in empty until `ConversationSync`.
        if is_creation {
            let _events =
                steward_list.install_list(0, std::slice::from_ref(&self_identity_bytes), 1, 0)?;
        }

        let mut scoring = self
            .plugins
            .conversation_plugins
            .make_scoring(&self.plugins.default_scoring_config);
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
        let session = Arc::new(RwLock::new(SessionRunner::new(
            conversation_name.to_string(),
            conversation,
            mls_opt,
            state_machine,
            phase_timer,
            config,
            scoring,
            steward_list,
            consensus,
            Arc::clone(&self.transport),
            Arc::from(self.identity.identity_bytes()),
            Arc::from(self.identity.identity_display()),
            Arc::from(self.app_id.as_slice()),
        )));
        {
            let mut conversations = self
                .conversations
                .write()
                .map_err(|_| UserError::LockPoisoned("conversation registry"))?;
            if conversations.contains_key(conversation_name) {
                return Err(UserError::ConversationAlreadyExists);
            }
            conversations.insert(conversation_name.to_string(), Arc::clone(&session));
        }

        // Record the lifecycle event first so integrators draining
        // [`User::drain_lifecycle_events`] see `Created` before any
        // per-session event emitted below.
        self.emit_lifecycle(ConversationLifecycle::Created(
            conversation_name.to_string(),
        ));
        session
            .read_or_err("session")?
            .emit_event(SessionEvent::PhaseChange(initial_state));

        Ok(())
    }

    /// Leave the conversation. `PendingJoin` short-circuits to local
    /// teardown (no MLS state yet). Otherwise delegates the protocol work
    /// to the session-side `initiate_self_leave` — opens a self-leave
    /// consensus session with the leaver's YES bundled at submit. We stay
    /// active until the next steward commit merges the removal; on that
    /// commit `ProcessResult::LeaveConversation` fires.
    pub async fn leave_conversation(&mut self, conversation_name: &str) -> Result<(), UserError> {
        info!(conversation = conversation_name, "leaving conversation");

        let entry_arc = self
            .lookup_entry(conversation_name)?
            .ok_or(UserError::ConversationNotFound)?;

        let is_pending_join = entry_arc.read_or_err("session")?.handle.current_state()
            == ConversationState::PendingJoin;
        if is_pending_join {
            entry_arc
                .read_or_err("session")?
                .emit_event(SessionEvent::Leaving);
            // Cancel auto-vote timers before removing the registry entry —
            // see `finalize_self_leave` for the rationale.
            self.cleanup_consensus_scope(conversation_name).await?;
            self.conversations
                .write()
                .map_err(|_| UserError::LockPoisoned("conversation registry"))?
                .remove(conversation_name);
            self.emit_lifecycle(ConversationLifecycle::Removed(
                conversation_name.to_string(),
            ));
            return Ok(());
        }

        SessionRunner::initiate_self_leave(&entry_arc).await
    }
}
