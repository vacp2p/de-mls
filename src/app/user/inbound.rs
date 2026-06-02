//! User-side inbound entry point.
//!
//! `process_inbound_packet` owns echo dedup + name routing.
//! `WELCOME_SUBTOPIC` packets carry [`MemberInvite`] — a peer
//! broadcasting their own key package so existing members can propose
//! adding them. App-message packets are handed off to
//! [`SessionRunner::dispatch_inbound_result`] for MLS processing and
//! per-conversation dispatch. Raw MLS welcomes never traverse the wire
//! here — they enter the library through [`User::accept_welcome`].

use std::sync::{Arc, RwLock};

use prost::Message;
use tracing::{debug, info};

use crate::{
    app::{DispatchOutcome, LockExt, SessionRunner, SessionTick, User, UserError},
    core::{
        ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory, CoreError,
        ProcessResult,
    },
    ds::{APP_MSG_SUBTOPIC, InboundPacket, WELCOME_SUBTOPIC},
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, MemberInvite, conversation_update_request,
    },
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    // ── Public API ───────────────────────────────────────────────────

    /// Process an inbound packet. The User-level entry point owns echo
    /// dedup, name-based routing, and the welcome subtopic's plug-in-
    /// factory access. App-message packets are handed off to the session
    /// for MLS processing and dispatch.
    pub async fn process_inbound_packet(
        &self,
        packet: InboundPacket,
    ) -> Result<SessionTick, UserError> {
        let conversation_id = packet.conversation_id.clone();

        // Echo dedup: drop our own messages received back from pub/sub.
        if packet.app_id.as_slice() == &*self.app_id {
            return Ok(SessionTick::empty());
        }

        let entry_arc = self
            .lookup_entry(&conversation_id)?
            .ok_or(UserError::ConversationNotFound)?;

        match packet.subtopic.as_str() {
            WELCOME_SUBTOPIC => {
                self.process_key_package_broadcast(&conversation_id, &packet.payload, &entry_arc)
                    .await?;
                Ok(entry_arc.read_or_err("session")?.tick())
            }
            APP_MSG_SUBTOPIC => {
                let result = {
                    let mut entry = entry_arc.write_or_err("session")?;
                    if entry.conversation.mls().is_none() {
                        // PendingJoin: no MLS to decrypt with yet. Dropped,
                        // not buffered — replay is a separate recovery track.
                        debug!(
                            conversation = conversation_id,
                            "app-message dropped: MLS not attached (still PendingJoin)"
                        );
                        return Ok(SessionTick::empty());
                    }
                    entry.conversation.process_inbound(&packet.payload)?
                };
                self.finish_dispatch(&conversation_id, &entry_arc, result)
                    .await?;
                Ok(entry_arc.read_or_err("session")?.tick())
            }
            other => Err(UserError::Core(CoreError::InvalidSubtopic(
                other.to_string(),
            ))),
        }
    }

    /// User-side completion of `LeaveConversation`: drop the entry from
    /// the registry, clean up the consensus scope, and broadcast removal.
    /// The session-side teardown (emit `Leaving`, delete MLS state) runs
    /// inside `SessionRunner::dispatch_inbound_result` /
    /// [`SessionRunner::poll_freeze_status`] /
    /// [`SessionRunner::check_pending_join`]; this method is the cleanup
    /// callers run when those signal "registry should be removed"
    /// (`DispatchOutcome::LeaveRequested` or `PendingJoinTick::Expired`).
    pub async fn finalize_self_leave(&self, conversation_id: &str) -> Result<(), UserError> {
        // Clean up (and cancel timers) before removing the entry — the
        // cleanup finds the runner via `lookup_entry`, so the entry must
        // still exist. Eviction and `Removed` are unconditional: a
        // scope-delete failure (timers already cancelled) must not strand a
        // zombie. The cleanup error surfaces after the conversation is gone.
        let cleanup = self.cleanup_consensus_scope(conversation_id).await;
        self.conversations
            .write()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .remove(conversation_id);
        self.emit_lifecycle(ConversationLifecycle::Removed(conversation_id.to_string()));
        cleanup
    }

    // ── Private ──────────────────────────────────────────────────────

    /// Decode a key-package broadcast off `WELCOME_SUBTOPIC` and, if the
    /// holder isn't already a member, promote it to a `MemberInvite`
    /// membership-change request. Raw MLS welcomes do not flow here —
    /// they enter through [`User::accept_welcome`].
    async fn process_key_package_broadcast(
        &self,
        conversation_id: &str,
        payload: &[u8],
        entry_arc: &Arc<RwLock<SessionRunner<P, CP>>>,
    ) -> Result<(), UserError> {
        let invite = MemberInvite::decode(payload)?;

        let already_member = {
            let entry = entry_arc.read_or_err("session")?;
            entry
                .conversation
                .mls()
                .map(|m| m.is_member(&invite.member_id))
                .unwrap_or(false)
        };
        if already_member {
            info!(
                conversation = conversation_id,
                member = ?invite.member_id,
                "key package skipped: already a member"
            );
            return Ok(());
        }

        info!(
            conversation = conversation_id,
            member = ?invite.member_id,
            "key package received"
        );

        let gur = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::MemberInvite(invite)),
        };
        SessionRunner::handle_incoming_update_request(entry_arc, gur).await
    }

    /// Drive the session-side dispatcher and finish lifecycle work on the
    /// User side when the session signals `LeaveRequested`.
    pub(crate) async fn finish_dispatch(
        &self,
        conversation_id: &str,
        entry_arc: &Arc<RwLock<SessionRunner<P, CP>>>,
        result: ProcessResult,
    ) -> Result<(), UserError> {
        let outcome = SessionRunner::dispatch_inbound_result(entry_arc, result).await?;
        if matches!(outcome, DispatchOutcome::LeaveRequested) {
            self.finalize_self_leave(conversation_id).await?;
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
    #[tokio::test]
    async fn finalize_self_leave_clears_pending_auto_votes() {
        let transport: SharedDeliveryService = Arc::new(Mutex::new(NullTransport));
        let mut user = make_user_from_private_key(ALICE_KEY, transport);
        user.start_conversation("test-conv", true).await.unwrap();

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

        user.finalize_self_leave("test-conv").await.unwrap();

        // Session entry is gone from the registry, so the conversation's
        // pending auto-votes can no longer fire from a poll cycle on this
        // user.
        assert!(
            user.lookup_entry("test-conv").unwrap().is_none(),
            "registry entry must be evicted on self-leave"
        );
    }
}
