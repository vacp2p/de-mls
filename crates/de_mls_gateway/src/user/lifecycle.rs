//! Create and leave operations for a conversation.

use tracing::info;

use std::sync::Arc;

use de_mls::{ConsensusPlugin, Conversation, ConversationConfig, LeaveOutcome};

use openmls_traits::signatures::Signer;

use crate::mls::GATEWAY_SUITE;
use crate::user::{LockExt, User, UserError};

impl<P: ConsensusPlugin, Sig: Signer + Clone> User<P, Sig> {
    /// Create a conversation we steward: seed the group from our credential and
    /// register it in `Working`. Seeding needs the concrete factory, so this
    /// path is concrete. Joiners hold no conversation until a welcome arrives —
    /// they reach one through [`User::accept_welcome`], not here.
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
        self.register_conversation(conversation_id, config)
    }

    /// Build and register the conversation we create; the factory seeds the
    /// group's leaf from our credential. The joiner side never lands here — it
    /// builds straight from a welcome in [`User::accept_welcome`].
    pub(crate) fn register_conversation(
        &mut self,
        conversation_id: &str,
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

        let factory = &self.plugins.conversation_plugins;
        let scoring = factory.make_scoring(&self.plugins.default_scoring_config);
        let steward = factory.make_steward(
            self.member_id.member_id_bytes(),
            self.plugins.default_steward_list_config.clone(),
        );
        let conversation = Conversation::create(
            conversation_id,
            factory.creator_provider(),
            factory.credential(),
            GATEWAY_SUITE,
            &self.signer,
            scoring,
            steward,
            self.plugins.consensus.build_service(),
            Arc::from(self.app_id.as_slice()),
            config,
            self.member_id.member_id_bytes(),
        )?;
        self.register_built(conversation_id, conversation)?;
        Ok(())
    }
}

impl<P: ConsensusPlugin, Sig: Signer + Clone> User<P, Sig> {
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
