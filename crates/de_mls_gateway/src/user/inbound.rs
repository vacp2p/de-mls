//! User-side inbound entry points.
//!
//! de-mls carries no transport subtopic: the integrator routes its own
//! channels here. Conversation traffic (chat / vote / commit / sync — the
//! envelope self-identifies) goes to [`User::handle_inbound`]; a joiner's
//! key-package announcement goes to [`User::receive_key_package`], which the
//! epoch steward relays as an Add proposal. Raw MLS welcomes enter through
//! [`User::accept_welcome`].

use de_mls::{ConsensusPlugin, DispatchOutcome, protos::de_mls::messages::v1::MemberInvite};
use prost::Message;

use openmls_traits::signatures::Signer;

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

impl<P: ConsensusPlugin, Sig: Signer + Clone> User<P, Sig> {
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
        let outcome = {
            let mut slot = entry_arc.write_or_err("conversation")?;
            // A pending-join slot has no live conversation yet — it can't process
            // conversation traffic. Dropping it is benign, not a failure.
            let Ok(conversation) = slot.live_mut() else {
                return Ok(());
            };
            conversation.process_inbound(
                self.plugins.conversation_plugins.provider(),
                &inbound.sender,
                &inbound.payload,
                &self.signer,
            )?
        };
        if matches!(outcome, DispatchOutcome::LeaveRequested) {
            self.finalize_self_leave(&inbound.conversation_id)?;
        }
        self.flush(&entry_arc)?;
        Ok(())
    }

    /// Ingest a joiner's key-package announcement: decode the `MemberInvite`
    /// and hand it to [`de_mls::Conversation::sponsor_member`], which relays it
    /// as an Add proposal only if we are the epoch steward — de-mls owns the
    /// steward-only-relay and unbundled-vote policy. (An explicit invite via
    /// [`User::add_member`] stays open to any member and bundles YES: that is
    /// one deliberate, endorsed action, not a broadcast fan-out.) Self-echoes
    /// are dropped before the registry lookup (same rationale as
    /// [`Self::handle_inbound`]); an announcement for a conversation we don't
    /// hold — or aren't yet joined into — is silently ignored.
    pub fn receive_key_package(&self, inbound: Inbound) -> Result<(), UserError> {
        if inbound.sender == self.app_id {
            return Ok(());
        }
        let Some(entry_arc) = self.lookup_entry(&inbound.conversation_id)? else {
            return Ok(());
        };
        let invite = match MemberInvite::decode(inbound.payload.as_slice()) {
            Ok(invite) => invite,
            Err(_) => return Ok(()),
        };
        {
            let mut slot = entry_arc.write_or_err("conversation")?;
            // Not yet joined ourselves (pending slot) — nothing to relay.
            let Ok(conversation) = slot.live_mut() else {
                return Ok(());
            };
            conversation.sponsor_member(
                self.plugins.conversation_plugins.provider(),
                &invite.key_package_bytes,
                &self.signer,
            )?;
        }
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
