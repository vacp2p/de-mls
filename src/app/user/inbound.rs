//! User-side inbound entry point.
//!
//! `process_inbound_packet` owns echo dedup + name routing; the welcome
//! subtopic handler reaches the per-conv plugin factory (`welcome_mls`),
//! which lives at the User layer. App-message packets are handed off to
//! [`SessionRunner::dispatch_inbound_result`] for MLS processing and
//! per-conversation dispatch.

use std::sync::{Arc, RwLock};

use prost::Message;
use tracing::info;

use crate::{
    app::{DispatchOutcome, LockExt, SessionRunner, User, UserError},
    core::{
        ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory, CoreError,
        ProcessResult, StewardListPlugin,
    },
    ds::{APP_MSG_SUBTOPIC, InboundPacket, WELCOME_SUBTOPIC},
    identity::ShortId,
    mls_crypto::{MlsService, key_package_bytes_from_json},
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, InviteMember, WelcomeMessage, conversation_update_request,
        welcome_message,
    },
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    // ── Public API ───────────────────────────────────────────────────

    /// Process an inbound packet. The User-level entry point owns echo
    /// dedup, name-based routing, and the welcome subtopic's plug-in-
    /// factory access. App-message packets are handed off to the session
    /// for MLS processing and dispatch.
    pub async fn process_inbound_packet(&self, packet: InboundPacket) -> Result<(), UserError> {
        let conversation_name = packet.conversation_id.clone();

        // Echo dedup: drop our own messages received back from pub/sub.
        if packet.app_id.as_slice() == &*self.app_id {
            return Ok(());
        }

        let entry_arc = self
            .lookup_entry(&conversation_name)?
            .ok_or(UserError::ConversationNotFound)?;

        match packet.subtopic.as_str() {
            WELCOME_SUBTOPIC => {
                self.process_welcome_packet(&conversation_name, &packet.payload, &entry_arc)
                    .await
            }
            APP_MSG_SUBTOPIC => {
                let result = {
                    let mut entry = entry_arc.write_or_err("session")?;
                    if entry.handle.mls().is_none() {
                        return Ok(());
                    }
                    entry.handle.process_inbound(&packet.payload)?
                };
                self.finish_dispatch(&conversation_name, &entry_arc, result)
                    .await
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
    pub async fn finalize_self_leave(&self, conversation_name: &str) -> Result<(), UserError> {
        // Cancel auto-vote timers before removing the registry entry —
        // `cleanup_consensus_scope` finds the runner via `lookup_entry` and
        // aborts its timers. If the entry is gone first, the lookup returns
        // `None` and the timers leak (still scheduled, will fire against a
        // conversation we've left).
        self.cleanup_consensus_scope(conversation_name).await?;
        self.conversations
            .write()
            .map_err(|_| UserError::LockPoisoned("conversation registry"))?
            .remove(conversation_name);
        let _ = self.lifecycle.send(ConversationLifecycle::Removed(
            conversation_name.to_string(),
        ));
        Ok(())
    }

    // ── Private ──────────────────────────────────────────────────────

    /// Welcome-subtopic dispatch. Two payload kinds:
    /// - `UserKeyPackage` — a peer wants to join. If we already have an MLS
    ///   service for this conversation and the candidate isn't a member, surface
    ///   it as a membership-change request.
    /// - `InvitationToJoin` — try the welcome factory. If it returns
    ///   `Some(svc)`, attach to the runner and fire the join flow.
    async fn process_welcome_packet(
        &self,
        conversation_name: &str,
        payload: &[u8],
        entry_arc: &Arc<RwLock<SessionRunner<P, CP>>>,
    ) -> Result<(), UserError> {
        let welcome_msg = WelcomeMessage::decode(payload)?;
        match welcome_msg.payload {
            Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;

                let already_member = {
                    let entry = entry_arc.read_or_err("session")?;
                    entry
                        .handle
                        .mls()
                        .map(|m| m.is_member(&identity))
                        .unwrap_or(false)
                };
                if already_member {
                    info!(
                        conversation = conversation_name,
                        identity = %ShortId::new(&identity),
                        "key package skipped: already a member"
                    );
                    return Ok(());
                }

                info!(
                    conversation = conversation_name,
                    identity = %ShortId::new(&identity),
                    "key package received"
                );

                let gur = ConversationUpdateRequest {
                    payload: Some(conversation_update_request::Payload::InviteMember(
                        InviteMember {
                            key_package_bytes,
                            identity,
                        },
                    )),
                };
                SessionRunner::handle_incoming_update_request(entry_arc, gur)
            }
            Some(welcome_message::Payload::InvitationToJoin(invitation)) => {
                let self_id = self.self_identity();
                let already_in = {
                    let entry = entry_arc.read_or_err("session")?;
                    entry.handle.steward_list.is_steward(self_id) || entry.handle.mls().is_some()
                };
                if already_in {
                    return Ok(());
                }

                let svc = self
                    .plugins
                    .conversation_plugins
                    .welcome_mls(&invitation.mls_message_out_bytes)?;
                let Some(svc) = svc else {
                    // Welcome wasn't for us.
                    return Ok(());
                };
                let joined_name = svc.conversation_id().to_string();
                {
                    let mut entry = entry_arc.write_or_err("session")?;
                    entry.handle.attach_mls(svc);
                }
                info!(
                    conversation = conversation_name,
                    "joined conversation via welcome"
                );
                self.finish_dispatch(
                    conversation_name,
                    entry_arc,
                    ProcessResult::JoinedConversation(joined_name),
                )
                .await
            }
            None => Ok(()),
        }
    }

    /// Drive the session-side dispatcher and finish lifecycle work on the
    /// User side when the session signals `LeaveRequested`.
    async fn finish_dispatch(
        &self,
        conversation_name: &str,
        entry_arc: &Arc<RwLock<SessionRunner<P, CP>>>,
        result: ProcessResult,
    ) -> Result<(), UserError> {
        let outcome = SessionRunner::dispatch_inbound_result(entry_arc, result).await?;
        if matches!(outcome, DispatchOutcome::LeaveRequested) {
            self.finalize_self_leave(conversation_name).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ds::{DeliveryService, DeliveryServiceError, InboundPacket, OutboundPacket};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Transport stub: `send` is a no-op so an outbound never reaches a
    /// real network; `subscribe` returns a dangling receiver.
    struct NullTransport;
    impl DeliveryService for NullTransport {
        fn send(&self, _: OutboundPacket) -> Result<String, DeliveryServiceError> {
            Ok("noop".into())
        }
        fn subscribe(&self) -> std::sync::mpsc::Receiver<InboundPacket> {
            let (_tx, rx) = std::sync::mpsc::channel();
            rx
        }
    }

    const ALICE_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    /// Self-leave must abort auto-vote timers — otherwise they fire
    /// against a conversation we've left.
    #[tokio::test]
    async fn finalize_self_leave_cancels_registered_auto_votes() {
        let transport: Arc<dyn DeliveryService> = Arc::new(NullTransport);
        let mut user = User::with_private_key(ALICE_KEY, transport).unwrap();
        user.start_conversation("test-conv", true).await.unwrap();

        let session = user
            .lookup_entry("test-conv")
            .unwrap()
            .expect("creator session registered");

        let executed = Arc::new(AtomicBool::new(false));
        let canary = {
            let executed = Arc::clone(&executed);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                executed.store(true, Ordering::SeqCst);
            })
        };
        session
            .read()
            .unwrap()
            .auto_vote_timers
            .lock()
            .unwrap()
            .insert(42, canary);

        user.finalize_self_leave("test-conv").await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert!(
            !executed.load(Ordering::SeqCst),
            "auto-vote timer must be aborted on self-leave"
        );
    }
}
