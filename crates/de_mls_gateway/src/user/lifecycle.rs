//! Create and leave operations for a conversation.

use std::sync::{Arc, RwLock};

use tracing::info;

use de_mls::{
    core::{ConsensusPlugin, ConversationConfig, ConversationPluginsFactory},
    session::{Conversation, ConversationDeps, LeaveOutcome},
};

use crate::user::{ConversationLifecycle, LockExt, User, UserError};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    pub fn start_conversation(
        &mut self,
        conversation_id: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
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
    ) -> Result<(), UserError> {
        if self
            .conversations
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .contains_key(conversation_id)
        {
            return Err(UserError::ConversationAlreadyExists);
        }

        let deps = ConversationDeps {
            plugins: &self.plugins.conversation_plugins,
            consensus: self.plugins.consensus.build_service(),
            identity: self.member_id.as_ref(),
            app_id: Arc::from(self.app_id.as_slice()),
            config,
            scoring_config: self.plugins.default_scoring_config.clone(),
            steward_list_config: self.plugins.default_steward_list_config.clone(),
        };
        let conversation = if is_creation {
            Conversation::create(conversation_id, deps)?
        } else {
            Conversation::join(conversation_id, deps)?
        };
        let entry = Arc::new(RwLock::new(conversation));
        {
            let mut conversations = self
                .conversations
                .write()
                .map_err(|_| UserError::LockPoisoned("conversation registry"))?;
            if conversations.contains_key(conversation_id) {
                return Err(UserError::ConversationAlreadyExists);
            }
            conversations.insert(conversation_id.to_string(), entry);
        }

        // The conversation already buffered its opening `PhaseChange`; record the
        // lifecycle event so integrators draining
        // [`User::drain_lifecycle_events`] discover the conversation.
        self.emit_lifecycle(ConversationLifecycle::Created(conversation_id.to_string()));

        Ok(())
    }

    /// Leave the conversation. Delegates to [`Conversation::leave`]: in
    /// `PendingJoin` the conversation tears down locally and returns `TornDown`;
    /// otherwise it opens a self-leave consensus round and returns
    /// `LeaveInitiated`. On `TornDown` this method finalises the User-side
    /// registry cleanup via `finalize_self_leave`.
    pub fn leave_conversation(&mut self, conversation_id: &str) -> Result<(), UserError> {
        info!(conversation = conversation_id, "leaving conversation");

        let entry_arc = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;

        let outcome = entry_arc.write_or_err("conversation")?.leave()?;
        match outcome {
            LeaveOutcome::TornDown => {
                self.finalize_self_leave(conversation_id)?;
            }
            LeaveOutcome::LeaveInitiated => {
                self.flush(&entry_arc)?;
            }
        }
        Ok(())
    }
}
