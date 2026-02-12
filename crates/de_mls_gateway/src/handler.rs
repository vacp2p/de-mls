//! Gateway implementation of GroupEventHandler.
//!
//! Bridges output events from the User to the gateway's event pipe
//! (delivery service for outbound, AppEvent for UI notifications).

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedSender;
use std::sync::Arc;
use tokio::task::spawn_blocking;

use de_mls::{
    core::{CoreError, GroupEventHandler, MessageType, message_types},
    ds::{DeliveryService, OutboundPacket, TopicFilter},
    protos::de_mls::messages::v1::{AppMessage, ConversationMessage, app_message},
};
use de_mls_ui_protocol::v1::AppEvent;

/// Event handler for the gateway layer.
///
/// Sends outbound packets via the delivery service and forwards
/// application messages to the UI event pipe.
pub struct GatewayEventHandler<DS: DeliveryService> {
    pub delivery: Arc<DS>,
    pub evt_tx: UnboundedSender<AppEvent>,
    pub topics: Arc<TopicFilter>,
}

#[async_trait]
impl<DS: DeliveryService> GroupEventHandler for GatewayEventHandler<DS> {
    async fn on_outbound(
        &self,
        _group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CoreError> {
        let delivery = self.delivery.clone();
        spawn_blocking(move || {
            delivery
                .send(packet)
                .map_err(|e| CoreError::DeliveryError(e.to_string()))
        })
        .await
        .map_err(|e| CoreError::DeliveryError(e.to_string()))?
    }

    async fn on_app_message(
        &self,
        _group_name: &str,
        message: AppMessage,
    ) -> Result<(), CoreError> {
        forward_app_message(&self.evt_tx, message)
            .map_err(|e| CoreError::HandlerError(e.to_string()))
    }

    async fn on_leave_group(&self, group_name: &str) -> Result<(), CoreError> {
        self.topics.remove_many(group_name).await;

        let _ = self
            .evt_tx
            .unbounded_send(AppEvent::GroupRemoved(group_name.to_string()));

        let _ = self
            .evt_tx
            .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                message: format!("You're removed from the group {group_name}").into_bytes(),
                sender: "system".to_string(),
                group_name: group_name.to_string(),
            }));

        Ok(())
    }

    async fn on_joined_group(&self, _group_name: &str) -> Result<(), CoreError> {
        Ok(())
    }

    async fn on_error(&self, group_name: &str, operation: &str, error: &str) {
        let _ = self.evt_tx.unbounded_send(AppEvent::Error(format!(
            "{operation} failed for group {group_name}: {error}"
        )));
    }
}

// Implement StateChangeHandler for app-layer state notifications
use de_mls::app::{GroupState, StateChangeHandler};

#[async_trait]
impl<DS: DeliveryService> StateChangeHandler for GatewayEventHandler<DS> {
    async fn on_state_changed(&self, group_name: &str, state: GroupState) {
        let _ = self.evt_tx.unbounded_send(AppEvent::GroupStateChanged {
            group_id: group_name.to_string(),
            state: state.to_string(),
        });
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
                        group_id: vp.group_id.clone(),
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
                group_name: cm.group_name.clone(),
            }))
            .map_err(|e| anyhow::anyhow!("error sending chat message: {e}")),
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
