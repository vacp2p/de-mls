//! Gateway implementation of ConversationEventHandler.
//!
//! Bridges output events from the User to the gateway's event pipe
//! (delivery service for outbound, AppEvent for UI notifications).

use std::sync::Arc;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedSender;
use tokio::task::spawn_blocking;

use de_mls::{
    app::{MessageType, format_conversation_request, message_types},
    core::{CallbackError, ConversationEventHandler, ConversationState},
    ds::{DeliveryService, OutboundPacket, TopicFilter},
    protos::de_mls::messages::v1::{
        AppMessage, ConversationMessage, ConversationUpdateRequest, app_message,
    },
};
use de_mls_ui_protocol::v1::AppEvent;

use crate::{EpochHistoryStore, MAX_EPOCH_HISTORY, forwarder::display_batch};

/// Event handler for the gateway layer.
///
/// Sends outbound packets via the delivery service and forwards
/// application messages to the UI event pipe.
pub struct GatewayEventHandler<DS: DeliveryService> {
    pub delivery: Arc<DS>,
    pub evt_tx: UnboundedSender<AppEvent>,
    pub topics: Arc<TopicFilter>,
    pub epoch_history: EpochHistoryStore,
}

#[async_trait]
impl<DS: DeliveryService> ConversationEventHandler for GatewayEventHandler<DS> {
    async fn on_outbound(
        &self,
        _group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CallbackError> {
        let delivery = self.delivery.clone();
        spawn_blocking(move || {
            delivery
                .send(packet)
                .map_err(|e| CallbackError(e.to_string()))
        })
        .await
        .map_err(|e| CallbackError(e.to_string()))?
    }

    async fn on_app_message(
        &self,
        _group_name: &str,
        message: AppMessage,
    ) -> Result<(), CallbackError> {
        forward_app_message(&self.evt_tx, message).map_err(|e| CallbackError(e.to_string()))
    }

    async fn on_leave_conversation(&self, conversation_name: &str) -> Result<(), CallbackError> {
        self.topics.remove_many(conversation_name).await;
        self.epoch_history.lock().remove(conversation_name);

        let _ = self
            .evt_tx
            .unbounded_send(AppEvent::GroupRemoved(conversation_name.to_string()));

        let _ = self
            .evt_tx
            .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                message: format!("You're removed from the group {conversation_name}").into_bytes(),
                sender: "system".to_string(),
                conversation_name: conversation_name.to_string(),
            }));

        Ok(())
    }

    async fn on_joined_conversation(&self, _group_name: &str) -> Result<(), CallbackError> {
        Ok(())
    }

    async fn on_error(&self, conversation_name: &str, operation: &str, error: &str) {
        let _ = self.evt_tx.unbounded_send(AppEvent::Error(format!(
            "{operation} failed for group {conversation_name}: {error}"
        )));
    }

    async fn on_own_proposal_submitted(
        &self,
        conversation_name: &str,
        proposal_id: u32,
        request: &ConversationUpdateRequest,
    ) -> Result<(), CallbackError> {
        let (action, address) = format_conversation_request(request);
        let _ = self.evt_tx.unbounded_send(AppEvent::OwnProposalSubmitted {
            conversation_id: conversation_name.to_string(),
            proposal_id,
            action,
            address,
        });
        Ok(())
    }

    async fn on_phase_change(&self, conversation_name: &str, state: ConversationState) {
        let _ = self.evt_tx.unbounded_send(AppEvent::GroupStateChanged {
            conversation_id: conversation_name.to_string(),
            state: state.to_string(),
        });
    }

    async fn on_commit_applied(
        &self,
        conversation_name: &str,
        batch: Vec<ConversationUpdateRequest>,
    ) -> Result<(), CallbackError> {
        if batch.is_empty() {
            return Ok(());
        }
        let formatted: Vec<Vec<(String, String)>> = {
            let mut store = self.epoch_history.lock();
            let entry = store.entry(conversation_name.to_string()).or_default();
            if entry.len() >= MAX_EPOCH_HISTORY {
                entry.pop_front();
            }
            entry.push_back(batch);
            entry.iter().map(|b| display_batch(b)).collect()
        };
        let _ = self.evt_tx.unbounded_send(AppEvent::EpochHistory {
            conversation_id: conversation_name.to_string(),
            epochs: formatted,
        });
        Ok(())
    }
}

/// Dispatch an AppMessage to the appropriate AppEvent variant on the UI pipe.
pub fn forward_app_message(
    evt_tx: &UnboundedSender<AppEvent>,
    app_msg: AppMessage,
) -> anyhow::Result<()> {
    match &app_msg.payload {
        Some(app_message::Payload::VotePayload(vp)) => evt_tx
            .unbounded_send(AppEvent::VoteRequested(vp.clone()))
            .map_err(|e| anyhow::anyhow!("error sending vote requested event: {e}"))
            .and_then(|_| {
                evt_tx
                    .unbounded_send(AppEvent::CurrentEpochProposalsCleared {
                        conversation_id: vp.conversation_id.clone(),
                    })
                    .map_err(|e| {
                        anyhow::anyhow!("error sending clear current epoch proposals event: {e}")
                    })
            }),
        Some(app_message::Payload::ProposalAdded(pa)) => evt_tx
            .unbounded_send(AppEvent::from(pa.clone()))
            .map_err(|e| anyhow::anyhow!("error sending proposal added event: {e}")),
        Some(app_message::Payload::BanRequest(br)) => evt_tx
            .unbounded_send(AppEvent::from(br.clone()))
            .map_err(|e| anyhow::anyhow!("error sending proposal added event (ban request): {e}")),
        Some(app_message::Payload::ConversationMessage(cm)) => evt_tx
            .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                message: cm.message.clone(),
                sender: cm.sender.clone(),
                conversation_name: cm.conversation_name.clone(),
            }))
            .map_err(|e| anyhow::anyhow!("error sending chat message: {e}")),
        Some(app_message::Payload::ConversationSync(_)) => {
            // Conversation sync is handled by the app layer (dispatch_inbound_result),
            // not forwarded to the UI. Nothing to display.
            Ok(())
        }
        _ => {
            let msg_type = app_msg
                .payload
                .as_ref()
                .map(|payload| payload.message_type())
                .unwrap_or(message_types::UNKNOWN);
            evt_tx
                .unbounded_send(AppEvent::Error(format!("Invalid app message: {msg_type}")))
                .map_err(|e| anyhow::anyhow!("error sending invalid app message event: {e}"))
        }
    }
}
