//! Registry CRUD on the `User` side: per-conversation session lookup +
//! the conversation-lifecycle broadcast channel.

use tokio::sync::broadcast;

use crate::{
    app::{SessionEntry, User, UserError},
    core::{ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory},
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Look up a conversation runner. Returns `Ok(None)` when no runner is
    /// registered for `conversation_name`. Takes the outer read lock briefly
    /// to clone the inner `Arc`, then releases it before the caller
    /// acquires the runner's own lock.
    pub fn lookup_entry(
        &self,
        conversation_name: &str,
    ) -> Result<Option<SessionEntry<P, CP>>, UserError> {
        Ok(self
            .conversations
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .get(conversation_name)
            .cloned())
    }

    /// Names of every conversation registered on this `User`.
    pub fn list_conversations(&self) -> Result<Vec<String>, UserError> {
        Ok(self
            .conversations
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .keys()
            .cloned()
            .collect())
    }

    /// Subscribe to User-level conversation lifecycle events. Integrators
    /// use this to discover new sessions and subscribe to their per-session
    /// [`crate::core::SessionEvent`] streams. Each call returns a fresh
    /// receiver.
    pub fn subscribe_conversations(&self) -> broadcast::Receiver<ConversationLifecycle> {
        self.lifecycle.subscribe()
    }
}
