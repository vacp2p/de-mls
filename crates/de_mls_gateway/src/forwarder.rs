use ds::DeliveryService;

use std::sync::{atomic::Ordering, Arc};
use tracing::info;

use de_mls::{
    app::{CoreCtx, UserAction},
    core::{message_types, MessageType},
    protos::de_mls::messages::v1::{app_message, AppMessage, ConversationMessage},
};
use de_mls_ui_protocol::v1::AppEvent;
use futures::channel::mpsc::UnboundedSender;

use crate::{Gateway, UserRef};

impl<DS: DeliveryService> Gateway<DS> {
    /// Spawn the consensus event forwarder.
    ///
    /// This handles both UI notification (AppEvent::ProposalDecided) and
    /// user-side processing (handle_consensus_event → outbound packets).
    pub(crate) fn spawn_consensus_forwarder(&self, core: Arc<CoreCtx<DS>>, user: UserRef) {
        let evt_tx = self.evt_tx.clone();
        let mut rx = core.consensus.subscribe_to_events();

        tokio::spawn(async move {
            tracing::info!("gateway: consensus forwarder started");
            while let Ok((group_name, event)) = rx.recv().await {
                // Forward to UI
                let _ = evt_tx
                    .unbounded_send(AppEvent::ProposalDecided(group_name.clone(), event.clone()));

                // Let user handle the consensus event and send any outbound packets
                match user
                    .write()
                    .await
                    .handle_consensus_event(&group_name, event)
                    .await
                {
                    Ok(packets) => {
                        for packet in packets {
                            if let Err(e) = core.app_state.delivery.send(packet).await {
                                tracing::warn!("error sending consensus outbound: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("handle_consensus_event failed: {e}");
                    }
                }
            }
            tracing::info!("gateway: consensus forwarder ended");
        });
    }

    /// Spawn the pubsub forwarder once, after first successful login.
    ///
    /// Receives inbound packets from the delivery service, passes them to
    /// the user for processing, and acts on the resulting UserAction.
    pub(crate) fn spawn_delivery_service_forwarder(&self, core: Arc<CoreCtx<DS>>, user: UserRef) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let evt_tx = self.evt_tx.clone();

        tokio::spawn(async move {
            let mut rx = core.app_state.pubsub.subscribe();
            tracing::info!("gateway: pubsub forwarder started");

            while let Ok(pkt) = rx.recv().await {
                if !core.topics.contains(&pkt.group_id, &pkt.subtopic).await {
                    continue;
                }

                match user.write().await.process_inbound_packet(pkt).await {
                    Ok(action) => {
                        handle_user_action(action, &core, &evt_tx).await;
                    }
                    Err(e) => {
                        tracing::error!("process_inbound_packet failed: {e}");
                    }
                }
            }

            tracing::info!("gateway: pubsub forwarder ended");
        });
    }
}

/// Act on a UserAction returned by the user.
pub async fn handle_user_action<DS: DeliveryService>(
    action: UserAction,
    core: &CoreCtx<DS>,
    evt_tx: &UnboundedSender<AppEvent>,
) {
    match action {
        UserAction::Outbound(packet) => {
            if let Err(e) = core.app_state.delivery.send(packet).await {
                tracing::warn!("error sending outbound message: {e}");
            }
        }
        UserAction::SendToApp(app_msg) => {
            if let Err(e) = forward_app_message(evt_tx, app_msg) {
                tracing::warn!("error forwarding app message: {e}");
            }
        }
        UserAction::LeaveGroup(group_name) => {
            core.topics.remove_many(&group_name).await;
            info!("Leave group: {:?}", &group_name);

            let _ = evt_tx
                .unbounded_send(AppEvent::GroupRemoved(group_name.clone()))
                .map_err(|e| anyhow::anyhow!("error sending group removed event: {e}"));

            let _ = evt_tx
                .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                    message: format!("You're removed from the group {group_name}").into_bytes(),
                    sender: "system".to_string(),
                    group_name: group_name.clone(),
                }))
                .map_err(|e| anyhow::anyhow!("error sending chat message: {e}"));
        }
        UserAction::DoNothing => {}
    }
}

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
