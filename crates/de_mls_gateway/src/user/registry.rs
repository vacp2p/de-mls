//! Registry CRUD on the `User` side: conversation lookup.

use std::sync::{Arc, RwLock};

use de_mls::ConsensusPlugin;

use openmls_traits::signatures::Signer;

use crate::user::{ConversationEntry, ConversationSlot, User, UserError};

impl<P: ConsensusPlugin, Sig: Signer> User<P, Sig> {
    /// Look up a conversation. Returns `Ok(None)` when no entry is
    /// registered for `conversation_id`. Takes the outer read lock briefly
    /// to clone the inner `Arc`, then releases it before the caller
    /// acquires the conversation's own lock.
    pub fn lookup_entry(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationEntry<P>>, UserError> {
        Ok(self
            .conversations
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .get(conversation_id)
            .cloned())
    }

    /// Names of every conversation registered on this `User` — both live and
    /// still-joining. A pending-join slot is listed so the integrator can show
    /// a joining conversation before its welcome arrives.
    pub fn list_conversations(&self) -> Result<Vec<String>, UserError> {
        Ok(self
            .conversations
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .keys()
            .cloned()
            .collect())
    }

    /// Register a pending-join slot: we have announced a key package and await
    /// the welcome. The slot holds no live [`de_mls::Conversation`] yet, so the
    /// conversation lists and reports as "joining" until
    /// [`Self::accept_welcome`] fills it in. Errors with
    /// [`UserError::ConversationAlreadyExists`] if a slot already exists.
    pub fn begin_pending_join(&self, conversation_id: &str) -> Result<(), UserError> {
        let mut conversations = self
            .conversations
            .write()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?;
        if conversations.contains_key(conversation_id) {
            return Err(UserError::ConversationAlreadyExists);
        }
        conversations.insert(
            conversation_id.to_string(),
            Arc::new(RwLock::new(ConversationSlot::pending())),
        );
        Ok(())
    }

    /// Abandon a pending join that never received its welcome: drop the slot
    /// only if it is still pending. A slot that went live in the meantime (the
    /// welcome raced in) is left untouched. No-op if the slot is gone.
    pub fn abandon_pending_join(&self, conversation_id: &str) -> Result<(), UserError> {
        let mut conversations = self
            .conversations
            .write()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?;
        let Some(entry) = conversations.get(conversation_id).cloned() else {
            return Ok(());
        };
        if entry
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation"))?
            .is_pending()
        {
            conversations.remove(conversation_id);
        }
        Ok(())
    }
}
