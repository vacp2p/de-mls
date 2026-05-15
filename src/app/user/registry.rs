//! Registry CRUD on the `User` side: per-conversation session lookup +
//! the conversation-lifecycle broadcast channel.

use std::sync::Arc;

use tokio::sync::{RwLock, broadcast};

use crate::{
    app::{SessionRunner, User},
    core::{ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory},
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Look up a conversation runner. Returns `None` when no runner is
    /// registered for `conversation_name`. Takes the outer read lock briefly
    /// to clone the inner `Arc`, then releases it before the caller
    /// acquires the runner's own lock.
    pub async fn lookup_entry(
        &self,
        conversation_name: &str,
    ) -> Option<Arc<RwLock<SessionRunner<P, CP>>>> {
        self.conversations
            .read()
            .await
            .get(conversation_name)
            .cloned()
    }

    /// Names of every conversation registered on this `User`.
    pub async fn list_conversations(&self) -> Vec<String> {
        self.conversations.read().await.keys().cloned().collect()
    }

    /// Subscribe to User-level conversation lifecycle events. Integrators
    /// use this to discover new sessions and subscribe to their per-session
    /// [`crate::core::SessionEvent`] streams. Each call returns a fresh
    /// receiver.
    pub fn subscribe_conversations(&self) -> broadcast::Receiver<ConversationLifecycle> {
        self.lifecycle.subscribe()
    }
}
