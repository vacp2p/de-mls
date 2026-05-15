//! User-side inbound entry point.
//!
//! `process_inbound_packet` owns echo dedup + name routing; the welcome
//! subtopic handler reaches the per-conv plugin factory (`welcome_mls`),
//! which lives at the User layer. App-message packets are handed off to
//! [`SessionRunner::dispatch_inbound_result`] for MLS processing and
//! per-conversation dispatch.

use std::sync::Arc;

use prost::Message;
use tokio::sync::RwLock;
use tracing::info;

use crate::{
    app::{DispatchOutcome, SessionRunner, User, UserError},
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
            .lookup_entry(&conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;

        match packet.subtopic.as_str() {
            WELCOME_SUBTOPIC => {
                self.process_welcome_packet(&conversation_name, &packet.payload, &entry_arc)
                    .await
            }
            APP_MSG_SUBTOPIC => {
                let result = {
                    let mut entry = entry_arc.write().await;
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
                    let entry = entry_arc.read().await;
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
                SessionRunner::handle_incoming_update_request(entry_arc, gur).await
            }
            Some(welcome_message::Payload::InvitationToJoin(invitation)) => {
                let self_id = self.self_identity();
                let already_in = {
                    let entry = entry_arc.read().await;
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
                    let mut entry = entry_arc.write().await;
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

    /// User-side completion of `LeaveConversation`: drop the entry from
    /// the registry, clean up the consensus scope, and broadcast removal.
    /// The session-side teardown (emit `Leaving`, delete MLS state) runs
    /// inside [`SessionRunner::dispatch_inbound_result`] /
    /// [`SessionRunner::poll_freeze_status`] /
    /// [`SessionRunner::check_pending_join`]; this method is the cleanup
    /// callers run when those signal "registry should be removed"
    /// (`DispatchOutcome::LeaveRequested` or `PendingJoinTick::Expired`).
    pub async fn finalize_self_leave(&self, conversation_name: &str) -> Result<(), UserError> {
        self.conversations.write().await.remove(conversation_name);
        self.cleanup_consensus_scope(conversation_name).await?;
        let _ = self.lifecycle.send(ConversationLifecycle::Removed(
            conversation_name.to_string(),
        ));
        Ok(())
    }
}
