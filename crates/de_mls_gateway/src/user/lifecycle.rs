//! Create and leave operations for a conversation.

use std::sync::{Arc, RwLock};

use tracing::info;

use de_mls::{
    core::{
        ConsensusPlugin, ConversationConfig, ConversationLifecycle, ConversationPluginsFactory,
        SessionEvent,
    },
    session::{ConversationDeps, ConversationState, SessionRunner, SessionError},
};

use crate::user::{LockExt, User};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    pub fn start_conversation(
        &mut self,
        conversation_id: &str,
        is_creation: bool,
    ) -> Result<(), SessionError> {
        self.start_conversation_with_config(
            conversation_id,
            is_creation,
            self.plugins.default_conversation_config.clone(),
        )
    }

    /// Like [`Self::start_conversation`] but with a per-conversation config override.
    pub fn start_conversation_with_config(
        &mut self,
        conversation_id: &str,
        is_creation: bool,
        config: ConversationConfig,
    ) -> Result<(), SessionError> {
        if self
            .conversations
            .read()
            .map_err(|_| SessionError::LockPoisoned("conversation registry"))?
            .contains_key(conversation_id)
        {
            return Err(SessionError::ConversationAlreadyExists);
        }

        let deps = ConversationDeps {
            plugins: &self.plugins.conversation_plugins,
            consensus: &self.plugins.consensus,
            identity: self.member_id.as_ref(),
            app_id: Arc::from(self.app_id.as_slice()),
            config,
            scoring_config: self.plugins.default_scoring_config.clone(),
            steward_list_config: self.plugins.default_steward_list_config.clone(),
        };
        let runner = if is_creation {
            SessionRunner::create(conversation_id, deps)?
        } else {
            SessionRunner::join(conversation_id, deps)?
        };
        let session = Arc::new(RwLock::new(runner));
        {
            let mut conversations = self
                .conversations
                .write()
                .map_err(|_| SessionError::LockPoisoned("conversation registry"))?;
            if conversations.contains_key(conversation_id) {
                return Err(SessionError::ConversationAlreadyExists);
            }
            conversations.insert(conversation_id.to_string(), session);
        }

        // The runner already buffered its opening `PhaseChange`; record the
        // lifecycle event so integrators draining
        // [`User::drain_lifecycle_events`] discover the session.
        self.emit_lifecycle(ConversationLifecycle::Created(conversation_id.to_string()));

        Ok(())
    }

    /// Leave the conversation. `PendingJoin` short-circuits to local
    /// teardown (no MLS state yet). Otherwise delegates the protocol work
    /// to the session-side `initiate_self_leave` — opens a self-leave
    /// consensus session with the leaver's YES bundled at submit. We stay
    /// active until the next steward commit merges the removal; on that
    /// commit `ProcessResult::LeaveConversation` fires.
    pub fn leave_conversation(&mut self, conversation_id: &str) -> Result<(), SessionError> {
        info!(conversation = conversation_id, "leaving conversation");

        let entry_arc = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;

        let is_pending_join = entry_arc.read_or_err("session")?.get_conversation_state()
            == ConversationState::PendingJoin;
        if is_pending_join {
            entry_arc
                .read_or_err("session")?
                .emit_event(SessionEvent::Leaving);
            // Cancel auto-vote timers before removing the registry entry —
            // see `finalize_self_leave` for the rationale.
            self.cleanup_consensus_scope(conversation_id)?;
            self.conversations
                .write()
                .map_err(|_| SessionError::LockPoisoned("conversation registry"))?
                .remove(conversation_id);
            self.emit_lifecycle(ConversationLifecycle::Removed(conversation_id.to_string()));
            return Ok(());
        }

        entry_arc.write_or_err("session")?.initiate_self_leave()?;
        self.flush(&entry_arc)?;
        Ok(())
    }
}
