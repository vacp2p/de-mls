//! Registry CRUD on the `User` side: conversation lookup.

use de_mls::{ConsensusPlugin, ConversationPlugins};

use openmls_traits::signatures::Signer;

use crate::user::{ConversationEntry, User, UserError};

impl<P: ConsensusPlugin, CP: ConversationPlugins, Sig: Signer> User<P, CP, Sig> {
    /// Look up a conversation. Returns `Ok(None)` when no entry is
    /// registered for `conversation_id`. Takes the outer read lock briefly
    /// to clone the inner `Arc`, then releases it before the caller
    /// acquires the conversation's own lock.
    pub fn lookup_entry(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationEntry<P, CP>>, UserError> {
        Ok(self
            .conversations
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .get(conversation_id)
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
}
