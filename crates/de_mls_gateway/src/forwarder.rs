use ds::waku::WakuDeliveryService;

use std::sync::{atomic::Ordering, Arc};

use de_mls::app::CoreCtx;
use de_mls_ui_protocol::v1::AppEvent;

use crate::{Gateway, UserRef};

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

        tokio::spawn(async move {
            let mut rx = core.app_state.pubsub.subscribe();
            tracing::info!("gateway: pubsub forwarder started");

            while let Ok(pkt) = rx.recv().await {
                if !core.topics.contains(&pkt.group_id, &pkt.subtopic).await {
                    continue;
                }

                if let Err(e) = user.write().await.process_inbound_packet(pkt).await {
                    tracing::error!("process_inbound_packet failed: {e}");
                }
            }

            tracing::info!("gateway: pubsub forwarder ended");
        });
    }
}
