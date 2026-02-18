use futures::channel::mpsc::UnboundedSender;
use hex::ToHex;
use std::sync::{Arc, atomic::Ordering};

use de_mls::{
    ds::WakuDeliveryService, mls_crypto::format_wallet_address,
    protos::de_mls::messages::v1::group_update_request,
};
use de_mls_ui_protocol::v1::AppEvent;

use crate::{CoreCtx, Gateway, UserRef};

/// Push refreshed approved-queue and epoch-history events to the UI.
pub(crate) async fn push_consensus_state(
    user: &UserRef,
    evt_tx: &UnboundedSender<AppEvent>,
    group_name: &str,
) {
    // Approved queue
    if let Ok(proposals) = user
        .read()
        .await
        .get_approved_proposal_for_current_epoch(group_name)
        .await
    {
        let display: Vec<(String, String)> = proposals
            .into_iter()
            .filter_map(|p| match p.payload {
                Some(group_update_request::Payload::InviteMember(kp)) => {
                    Some(("Add Member".to_string(), kp.identity.encode_hex()))
                }
                Some(group_update_request::Payload::RemoveMember(id)) => {
                    Some(("Remove Member".to_string(), id.identity.encode_hex()))
                }
                Some(group_update_request::Payload::EmergencyCriteria(ec)) => {
                    let (label, target) = match ec.evidence.as_ref() {
                        Some(e) => (
                            format!("Emergency: {}", e.violation_type_label()),
                            format_wallet_address(&e.target_member_id),
                        ),
                        None => (
                            "Emergency: Unknown Violation".to_string(),
                            "unknown".to_string(),
                        ),
                    };
                    Some((label, target))
                }
                None => None,
            })
            .collect();
        let _ = evt_tx.unbounded_send(AppEvent::CurrentEpochProposals {
            group_id: group_name.to_string(),
            proposals: display,
        });
    }

    // Epoch history
    if let Ok(history) = user.read().await.get_epoch_history(group_name).await {
        let epochs: Vec<Vec<(String, String)>> = history
            .into_iter()
            .map(|batch| {
                batch
                    .into_iter()
                    .filter_map(|p| match p.payload {
                        Some(group_update_request::Payload::InviteMember(kp)) => {
                            Some(("Add Member".to_string(), kp.identity.encode_hex()))
                        }
                        Some(group_update_request::Payload::RemoveMember(id)) => {
                            Some(("Remove Member".to_string(), id.identity.encode_hex()))
                        }
                        Some(group_update_request::Payload::EmergencyCriteria(ec)) => {
                            let (label, target) = match ec.evidence.as_ref() {
                                Some(e) => (
                                    format!("Emergency: {}", e.violation_type_label()),
                                    format_wallet_address(&e.target_member_id),
                                ),
                                None => (
                                    "Emergency: Unknown Violation".to_string(),
                                    "unknown".to_string(),
                                ),
                            };
                            Some((label, target))
                        }
                        None => None,
                    })
                    .collect()
            })
            .collect();
        let _ = evt_tx.unbounded_send(AppEvent::EpochHistory {
            group_id: group_name.to_string(),
            epochs,
        });
    }
}

impl Gateway<WakuDeliveryService> {
    /// Spawn the consensus event forwarder.
    ///
    /// This handles both UI notification (AppEvent::ProposalDecided) and
    /// user-side processing (handle_consensus_event internally calls handler).
    pub(crate) fn spawn_consensus_forwarder(
        &self,
        core: Arc<CoreCtx<WakuDeliveryService>>,
        user: UserRef,
    ) {
        let evt_tx = self.evt_tx.clone();
        let mut rx = core.consensus.subscribe_to_events();

        tokio::spawn(async move {
            tracing::info!("gateway: consensus forwarder started");
            while let Ok((group_name, event)) = rx.recv().await {
                // Forward to UI
                let _ = evt_tx
                    .unbounded_send(AppEvent::ProposalDecided(group_name.clone(), event.clone()));

                if let Err(e) = user
                    .write()
                    .await
                    .handle_consensus_event(&group_name, event)
                    .await
                {
                    tracing::warn!("handle_consensus_event failed: {e}");
                }

                // Push refreshed approved queue + epoch history
                push_consensus_state(&user, &evt_tx, &group_name).await;
            }
            tracing::info!("gateway: consensus forwarder ended");
        });
    }

    /// Spawn the pubsub forwarder once, after first successful login.
    ///
    /// Receives inbound packets from the delivery service, passes them to
    /// the user for processing. Handler callbacks handle outbound and app events internally.
    pub(crate) fn spawn_delivery_service_forwarder(
        &self,
        core: Arc<CoreCtx<WakuDeliveryService>>,
        user: UserRef,
    ) {
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

                let group_id = pkt.group_id.clone();

                if let Err(e) = user.write().await.process_inbound_packet(pkt).await {
                    tracing::error!("process_inbound_packet failed: {e}");
                }

                // Push refreshed approved queue + epoch history
                // (covers batch processing clearing the queue)
                push_consensus_state(&user, &evt_tx, &group_id).await;
            }

            tracing::info!("gateway: pubsub forwarder ended");
        });
    }
}
