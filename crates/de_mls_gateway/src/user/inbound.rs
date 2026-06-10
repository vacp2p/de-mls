//! User-side inbound entry points.
//!
//! de-mls carries no transport subtopic: the integrator routes its own
//! channels here. Conversation traffic (chat / vote / commit / sync — the
//! envelope self-identifies) goes to [`User::handle_inbound`]; a joiner's
//! key-package announcement goes to [`User::receive_key_package`]. Both
//! delegate dedup and dispatch decisions to the runner. Raw MLS welcomes
//! enter through [`User::accept_welcome`].

use de_mls::{
    core::{ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory},
    session::{DispatchOutcome, SessionError, SessionTick},
};

use crate::user::{LockExt, User};

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

    /// Ingest conversation traffic (chat / vote / commit / sync). Echo-dedup
    /// and `PendingJoin`-drop are handled inside the runner. On
    /// `LeaveRequested` the session has completed its protocol-side teardown;
    /// this method finalises the User-side registry cleanup.
    pub fn handle_inbound(&self, inbound: Inbound) -> Result<SessionTick, SessionError> {
        let entry_arc = self
            .lookup_entry(&inbound.conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        let outcome = entry_arc
            .write_or_err("session")?
            .process_inbound(&inbound.sender, &inbound.payload)?;
        if matches!(outcome, DispatchOutcome::LeaveRequested) {
            self.finalize_self_leave(&inbound.conversation_id)?;
        }
        self.flush(&entry_arc)?;
        Ok(SessionTick {
            next_wakeup_in: entry_arc.read_or_err("session")?.next_wakeup_in(),
        })
    }

    /// Ingest a joiner's key-package announcement. Echo-dedup and admission
    /// decisions are handled inside the runner.
    pub fn receive_key_package(&self, inbound: Inbound) -> Result<SessionTick, SessionError> {
        let entry_arc = self
            .lookup_entry(&inbound.conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        entry_arc
            .write_or_err("session")?
            .receive_key_package(&inbound.sender, &inbound.payload)?;
        self.flush(&entry_arc)?;
        Ok(SessionTick {
            next_wakeup_in: entry_arc.read_or_err("session")?.next_wakeup_in(),
        })
    }

    /// User-side completion of `LeaveConversation`: drop the entry from
    /// the registry, clean up the consensus scope, and broadcast removal.
    /// The session-side teardown (emit `Leaving`, cancel timers, delete MLS
    /// state) runs inside the runner before this is called; this method is
    /// the cleanup callers run when the runner signals it has finished.
    pub fn finalize_self_leave(&self, conversation_id: &str) -> Result<(), SessionError> {
        // Scope cleanup before registry remove — the cleanup finds the runner
        // via lookup_entry, so the entry must still exist. Eviction and
        // `Removed` are unconditional: a scope-delete failure must not strand
        // a zombie.
        let cleanup = self.cleanup_consensus_scope(conversation_id);
        self.conversations
            .write()
            .map_err(|_| SessionError::LockPoisoned("conversation registry"))?
            .remove(conversation_id);
        self.emit_lifecycle(ConversationLifecycle::Removed(conversation_id.to_string()));
        cleanup
    }
}
