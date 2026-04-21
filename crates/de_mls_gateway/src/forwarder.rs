use futures::channel::mpsc::UnboundedSender;
use hashgraph_like_consensus::events::ConsensusEventBus;
use std::sync::{Arc, atomic::Ordering};

use de_mls::{
    app::format_group_request, ds::WakuDeliveryService, mls_crypto::format_wallet_address,
};
use de_mls_ui_protocol::v1::{AppEvent, MemberInfo};

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
            .iter()
            .filter(|p| p.payload.is_some())
            .map(|p| format_group_request(p))
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
                    .iter()
                    .filter(|p| p.payload.is_some())
                    .map(|p| format_group_request(p))
                    .collect()
            })
            .collect();
        let _ = evt_tx.unbounded_send(AppEvent::EpochHistory {
            group_id: group_name.to_string(),
            epochs,
        });
    }
}

/// Push refreshed member scores to the UI.
///
/// Called after consensus events that may have changed peer scores
/// (emergency criteria proposals produce score ops on resolution).
pub(crate) async fn push_member_scores(
    user: &UserRef,
    evt_tx: &UnboundedSender<AppEvent>,
    group_name: &str,
) {
    let user = user.read().await;
    let addresses = match user.get_group_members(group_name).await {
        Ok(a) => a,
        Err(_) => return,
    };
    let scores = user.get_member_scores(group_name);
    let roles = user.get_member_roles(group_name).await.unwrap_or_default();
    let members: Vec<MemberInfo> = addresses
        .into_iter()
        .map(|address| {
            let score = scores
                .iter()
                .find(|(raw_id, _)| format_wallet_address(raw_id.as_slice()) == address)
                .map(|(_, s)| *s)
                .unwrap_or(100);
            let role = roles
                .iter()
                .find(|(raw_id, _)| format_wallet_address(raw_id.as_slice()) == address)
                .map(|(_, r)| r.to_string())
                .unwrap_or_else(|| "member".to_string());
            MemberInfo {
                address,
                score,
                role,
            }
        })
        .collect();
    let _ = evt_tx.unbounded_send(AppEvent::GroupMembers {
        group_id: group_name.to_string(),
        members,
    });

    // Also push steward status — it may have changed after election or epoch advance.
    let is_steward = user.is_steward_for_group(group_name).await.unwrap_or(false);
    let _ = evt_tx.unbounded_send(AppEvent::StewardStatus {
        group_id: group_name.to_string(),
        is_steward,
    });
}

impl Gateway<WakuDeliveryService> {
    /// Spawn the consensus event forwarder.
    ///
    /// This handles both UI notification (AppEvent::ProposalDecided) and
    /// user-side processing (apply_consensus_outcome internally calls handler).
    pub(crate) fn spawn_consensus_forwarder(
        &self,
        core: Arc<CoreCtx<WakuDeliveryService>>,
        user: UserRef,
    ) {
        let evt_tx = self.evt_tx.clone();
        let mut rx = core.consensus.event_bus().subscribe();

        tokio::spawn(async move {
            tracing::info!("gateway: consensus forwarder started");
            while let Ok((group_name, event)) = rx.recv().await {
                // Forward to UI
                let _ = evt_tx
                    .unbounded_send(AppEvent::ProposalDecided(group_name.clone(), event.clone()));

                if let Err(e) = user
                    .write()
                    .await
                    .apply_consensus_outcome(&group_name, event)
                    .await
                {
                    tracing::warn!("apply_consensus_outcome failed: {e}");
                }

                // Push refreshed approved queue, epoch history, and member scores
                push_consensus_state(&user, &evt_tx, &group_name).await;
                push_member_scores(&user, &evt_tx, &group_name).await;
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

            loop {
                let pkt = match rx.recv().await {
                    Ok(pkt) => pkt,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("pubsub forwarder lagged, dropped {n} messages");
                        continue;
                    }
                    Err(_) => break,
                };
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
