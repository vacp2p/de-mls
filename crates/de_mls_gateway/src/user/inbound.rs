//! User-side inbound entry points.
//!
//! de-mls carries no transport subtopic: the integrator routes its own
//! channels here. Conversation traffic (chat / vote / commit / sync — the
//! envelope self-identifies) goes to [`User::handle_inbound`]; a joiner's
//! key-package announcement goes to [`User::receive_key_package`]. Both own
//! echo dedup and name-based routing to a session. Raw MLS welcomes never
//! traverse these — they enter the library through [`User::accept_welcome`].

use std::sync::{Arc, RwLock};

use tracing::debug;

use de_mls::{
    core::{ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory, ProcessResult},
    session::{DispatchOutcome, SessionRunner, SessionTick, SessionError},
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

    /// Ingest conversation traffic (chat / vote / commit / sync). The
    /// envelope self-identifies its kind, so no subtopic is needed; the
    /// payload is decrypted and dispatched on the addressed session. Drops
    /// self-echoes and packets for a conversation not yet MLS-attached
    /// (`PendingJoin`).
    pub fn handle_inbound(&self, inbound: Inbound) -> Result<SessionTick, SessionError> {
        if inbound.sender == self.app_id {
            return Ok(SessionTick::empty());
        }
        let entry_arc = self
            .lookup_entry(&inbound.conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        let result = {
            let mut entry = entry_arc.write_or_err("session")?;
            if !entry.has_mls() {
                // PendingJoin: no MLS to decrypt with yet. Dropped, not
                // buffered — replay is a separate recovery track.
                debug!(
                    conversation = inbound.conversation_id,
                    "inbound dropped: MLS not attached (still PendingJoin)"
                );
                return Ok(SessionTick::empty());
            }
            entry.process_inbound(&inbound.payload)?
        };
        self.finish_dispatch(&inbound.conversation_id, &entry_arc, result)?;
        self.flush(&entry_arc)?;
        Ok(entry_arc.read_or_err("session")?.tick())
    }

    /// Ingest a joiner's key-package announcement. The decision to admit the
    /// holder is a conversation decision, delegated to
    /// [`SessionRunner::receive_key_package`].
    pub fn receive_key_package(&self, inbound: Inbound) -> Result<SessionTick, SessionError> {
        if inbound.sender == self.app_id {
            return Ok(SessionTick::empty());
        }
        let entry_arc = self
            .lookup_entry(&inbound.conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        entry_arc
            .write_or_err("session")?
            .receive_key_package(&inbound.payload)?;
        self.flush(&entry_arc)?;
        Ok(entry_arc.read_or_err("session")?.tick())
    }

    /// User-side completion of `LeaveConversation`: drop the entry from
    /// the registry, clean up the consensus scope, and broadcast removal.
    /// The session-side teardown (emit `Leaving`, delete MLS state) runs
    /// inside `SessionRunner::dispatch_inbound_result` /
    /// [`SessionRunner::poll_freeze_status`] /
    /// [`SessionRunner::check_pending_join`]; this method is the cleanup
    /// callers run when those signal "registry should be removed"
    /// (`DispatchOutcome::LeaveRequested` or `PendingJoinTick::Expired`).
    pub fn finalize_self_leave(&self, conversation_id: &str) -> Result<(), SessionError> {
        // Clean up (and cancel timers) before removing the entry — the
        // cleanup finds the runner via `lookup_entry`, so the entry must
        // still exist. Eviction and `Removed` are unconditional: a
        // scope-delete failure (timers already cancelled) must not strand a
        // zombie. The cleanup error surfaces after the conversation is gone.
        let cleanup = self.cleanup_consensus_scope(conversation_id);
        self.conversations
            .write()
            .map_err(|_| SessionError::LockPoisoned("conversation registry"))?
            .remove(conversation_id);
        self.emit_lifecycle(ConversationLifecycle::Removed(conversation_id.to_string()));
        cleanup
    }

    // ── Private ──────────────────────────────────────────────────────

    /// Drive the session-side dispatcher and finish lifecycle work on the
    /// User side when the session signals `LeaveRequested`.
    pub(crate) fn finish_dispatch(
        &self,
        conversation_id: &str,
        entry_arc: &Arc<RwLock<SessionRunner<P, CP>>>,
        result: ProcessResult,
    ) -> Result<(), SessionError> {
        let outcome = entry_arc
            .write_or_err("session")?
            .dispatch_inbound_result(result)?;
        if matches!(outcome, DispatchOutcome::LeaveRequested) {
            self.finalize_self_leave(conversation_id)?;
        }
        Ok(())
    }
}
