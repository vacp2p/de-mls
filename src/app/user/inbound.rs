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

use crate::{
    app::{DispatchOutcome, LockExt, SessionRunner, SessionTick, User, UserError},
    core::{ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory, ProcessResult},
};

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
    pub fn handle_inbound(&self, inbound: Inbound) -> Result<SessionTick, UserError> {
        if inbound.sender == self.app_id {
            return Ok(SessionTick::empty());
        }
        let entry_arc = self
            .lookup_entry(&inbound.conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
        let result = {
            let mut entry = entry_arc.write_or_err("session")?;
            if entry.conversation.mls().is_none() {
                // PendingJoin: no MLS to decrypt with yet. Dropped, not
                // buffered — replay is a separate recovery track.
                debug!(
                    conversation = inbound.conversation_id,
                    "inbound dropped: MLS not attached (still PendingJoin)"
                );
                return Ok(SessionTick::empty());
            }
            entry.conversation.process_inbound(&inbound.payload)?
        };
        self.finish_dispatch(&inbound.conversation_id, &entry_arc, result)?;
        self.flush(&entry_arc)?;
        Ok(entry_arc.read_or_err("session")?.tick())
    }

    /// Ingest a joiner's key-package announcement. The decision to admit the
    /// holder is a conversation decision, delegated to
    /// [`SessionRunner::receive_key_package`].
    pub fn receive_key_package(&self, inbound: Inbound) -> Result<SessionTick, UserError> {
        if inbound.sender == self.app_id {
            return Ok(SessionTick::empty());
        }
        let entry_arc = self
            .lookup_entry(&inbound.conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;
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
    pub fn finalize_self_leave(&self, conversation_id: &str) -> Result<(), UserError> {
        // Clean up (and cancel timers) before removing the entry — the
        // cleanup finds the runner via `lookup_entry`, so the entry must
        // still exist. Eviction and `Removed` are unconditional: a
        // scope-delete failure (timers already cancelled) must not strand a
        // zombie. The cleanup error surfaces after the conversation is gone.
        let cleanup = self.cleanup_consensus_scope(conversation_id);
        self.conversations
            .write()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
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
    ) -> Result<(), UserError> {
        let outcome = entry_arc
            .write_or_err("session")?
            .dispatch_inbound_result(result)?;
        if matches!(outcome, DispatchOutcome::LeaveRequested) {
            self.finalize_self_leave(conversation_id)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    use crate::ds::{DeliveryService, DeliveryServiceError, OutboundPacket, SharedDeliveryService};
    use crate::test_fixtures::make_user_from_private_key;

    /// Transport stub: `publish` is a no-op so an outbound never reaches a
    /// real network; `subscribe` is a no-op too.
    #[derive(Debug)]
    struct NullTransport;
    impl DeliveryService for NullTransport {
        type Error = DeliveryServiceError;

        fn publish(&mut self, _: OutboundPacket) -> Result<(), Self::Error> {
            Ok(())
        }

        fn subscribe(&mut self, _delivery_address: &str) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    const ALICE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    /// Self-leave must abort auto-vote timers — otherwise they fire
    /// against a conversation we've left.
    /// Self-leave must drop the pending auto-vote registry — otherwise
    /// the next `tick_deadlines` would fire against a conversation
    /// we've left.
    #[test]
    fn finalize_self_leave_clears_pending_auto_votes() {
        let transport: SharedDeliveryService = Arc::new(Mutex::new(NullTransport));
        let mut user = make_user_from_private_key(ALICE_KEY, transport);
        user.start_conversation("test-conv", true).unwrap();

        let session = user
            .lookup_entry("test-conv")
            .unwrap()
            .expect("creator session registered");

        // Seed a pending auto-vote with a far-future fire-at so the
        // assertion isn't sensitive to wall-clock drift.
        session
            .write()
            .unwrap()
            .register_auto_vote(42, Duration::from_secs(600), true);
        assert!(
            session.read().unwrap().pending_auto_votes.contains_key(&42),
            "auto-vote must be registered before self-leave"
        );

        user.finalize_self_leave("test-conv").unwrap();

        // Session entry is gone from the registry, so the conversation's
        // pending auto-votes can no longer fire from a poll cycle on this
        // user.
        assert!(
            user.lookup_entry("test-conv").unwrap().is_none(),
            "registry entry must be evicted on self-leave"
        );
    }
}
