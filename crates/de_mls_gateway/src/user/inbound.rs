//! User-side inbound entry points.
//!
//! de-mls carries no transport subtopic: the integrator routes its own
//! channels here. Conversation traffic (chat / vote / commit / sync — the
//! envelope self-identifies) goes to [`User::handle_inbound`]; a joiner's
//! key-package announcement goes to [`User::receive_key_package`]. Both
//! delegate dedup and dispatch decisions to the conversation. Raw MLS welcomes
//! enter through [`User::accept_welcome`].

use de_mls::{
    core::{ConsensusPlugin, ConversationPluginsFactory},
    session::DispatchOutcome,
};

use crate::user::{ConversationLifecycle, LockExt, User, UserError};

/// A payload delivered from the network into the library, addressed to a
/// conversation. The integrator builds this from its own wire format and
/// routes it by its own channel knowledge — de-mls assigns no transport
/// subtopic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    pub conversation_id: String,
    /// Sender's application instance id, used for echo dedup.
    pub sender: Vec<u8>,
    pub payload: Vec<u8>,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    // ── Public API ───────────────────────────────────────────────────

    /// Ingest conversation traffic (chat / vote / commit / sync). Self-echoes
    /// are dropped before the registry lookup — our own packets can still
    /// arrive for a conversation we just left, and must not surface as
    /// `ConversationNotFound`. The conversation dedups again for direct integrators.
    /// On `LeaveRequested` the conversation has completed its protocol-side
    /// teardown; this method finalises the User-side registry cleanup.
    pub fn handle_inbound(&self, inbound: Inbound) -> Result<(), UserError> {
        if inbound.sender == self.app_id {
            return Ok(());
        }
        let entry_arc = self
            .lookup_entry(&inbound.conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        let outcome = entry_arc
            .write_or_err("conversation")?
            .process_inbound(&inbound.sender, &inbound.payload)?;
        if matches!(outcome, DispatchOutcome::LeaveRequested) {
            self.finalize_self_leave(&inbound.conversation_id)?;
        }
        self.flush(&entry_arc)?;
        Ok(())
    }

    /// Ingest a joiner's key-package announcement. Self-echoes are dropped
    /// before the registry lookup (same rationale as [`Self::handle_inbound`]);
    /// admission decisions are handled inside the conversation.
    pub fn receive_key_package(&self, inbound: Inbound) -> Result<(), UserError> {
        if inbound.sender == self.app_id {
            return Ok(());
        }
        let entry_arc = self
            .lookup_entry(&inbound.conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        entry_arc
            .write_or_err("conversation")?
            .receive_key_package(&inbound.sender, &inbound.payload)?;
        self.flush(&entry_arc)?;
        Ok(())
    }

    /// User-side completion of `LeaveConversation`: drop the entry from
    /// the registry, clean up the consensus scope, and broadcast removal.
    /// The conversation-side teardown (emit `Leaving`, cancel timers, delete MLS
    /// state) runs inside the conversation before this is called; this method is
    /// the cleanup callers run when the conversation signals it has finished.
    pub fn finalize_self_leave(&self, conversation_id: &str) -> Result<(), UserError> {
        // Scope cleanup before registry remove — the cleanup finds the conversation
        // via lookup_entry, so the entry must still exist. Eviction and
        // `Removed` are unconditional: a scope-delete failure must not strand
        // a zombie.
        let cleanup = self.cleanup_consensus_scope(conversation_id);
        self.conversations
            .write()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .remove(conversation_id);
        self.emit_lifecycle(ConversationLifecycle::Removed(conversation_id.to_string()));
        cleanup
    }
}
