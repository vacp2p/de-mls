//! Registry CRUD on the `User` side: per-conversation session lookup.

use de_mls::{
    core::{ConsensusPlugin, ConversationPluginsFactory},
    session::SessionError,
};

use crate::user::{SessionEntry, User};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Look up a conversation runner. Returns `Ok(None)` when no runner is
    /// registered for `conversation_id`. Takes the outer read lock briefly
    /// to clone the inner `Arc`, then releases it before the caller
    /// acquires the runner's own lock.
    pub fn lookup_entry(
        &self,
        conversation_id: &str,
    ) -> Result<Option<SessionEntry<P, CP>>, SessionError> {
        Ok(self
            .conversations
            .read()
            .map_err(|_| SessionError::LockPoisoned("conversation registry"))?
            .get(conversation_id)
            .cloned())
    }

    /// Names of every conversation registered on this `User`.
    pub fn list_conversations(&self) -> Result<Vec<String>, SessionError> {
        Ok(self
            .conversations
            .read()
            .map_err(|_| SessionError::LockPoisoned("conversation registry"))?
            .keys()
            .cloned()
            .collect())
    }
}
