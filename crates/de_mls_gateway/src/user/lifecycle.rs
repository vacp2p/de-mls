//! Create and leave operations for a conversation.

use tracing::info;

use de_mls::{
    ConsensusPlugin, Conversation, ConversationConfig, ConversationError,
    ConversationPluginsFactory, LeaveOutcome,
};

use openmls_traits::signatures::Signer;

use crate::mls::DefaultConversationPluginsFactory;
use crate::user::{LockExt, User, UserError};

impl<P: ConsensusPlugin, Sig: Signer + Clone> User<P, DefaultConversationPluginsFactory, Sig> {
    /// Create a conversation we steward: mint our own key package, seed the
    /// group, and register it in `Working`. Minting the creator key package
    /// needs the concrete factory, so this entry point is concrete; the build
    /// itself is generic via `register_conversation`. Joiners hold no
    /// conversation until a welcome arrives — they reach one through
    /// [`User::accept_welcome`], not here.
    pub fn start_conversation(&mut self, conversation_id: &str) -> Result<(), UserError> {
        self.start_conversation_with_config(
            conversation_id,
            self.plugins.default_conversation_config.clone(),
        )
    }

    /// Like [`Self::start_conversation`] but with a per-conversation config override.
    pub fn start_conversation_with_config(
        &mut self,
        conversation_id: &str,
        config: ConversationConfig,
    ) -> Result<(), UserError> {
        let key_package = self
            .generate_key_package()
            .map_err(ConversationError::from)?;
        self.register_conversation(conversation_id, key_package.as_bytes(), config)
    }
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory, Sig: Signer + Clone> User<P, CP, Sig> {
    /// Build and register the conversation we create, seeding the group with
    /// our own `key_package` leaf. The joiner side never lands here — it
    /// builds straight from a welcome in [`User::accept_welcome`].
    pub(crate) fn register_conversation(
        &mut self,
        conversation_id: &str,
        key_package: &[u8],
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

        let deps = self.build_deps(config);
        let conversation = Conversation::create(conversation_id, key_package, deps, &self.signer)?;
        self.register_built(conversation_id, conversation)?;
        Ok(())
    }

    /// Leave the conversation. Delegates to [`Conversation::leave`], which
    /// opens a self-leave consensus round and returns
    /// [`LeaveOutcome::LeaveInitiated`]; the User-side registry cleanup
    /// happens later, once the conversation signals teardown via
    /// `LeaveRequested` / `finalize_self_leave`. Flush publishes the opening
    /// self-leave proposal.
    pub fn leave_conversation(&mut self, conversation_id: &str) -> Result<(), UserError> {
        info!(conversation = conversation_id, "leaving conversation");

        let entry_arc = self
            .lookup_entry(conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;

        let LeaveOutcome::LeaveInitiated = entry_arc
            .write_or_err("conversation")?
            .leave(&self.signer)?;
        self.flush(&entry_arc)?;
        Ok(())
    }
}
