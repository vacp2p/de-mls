use std::sync::{Arc, atomic::Ordering};

use de_mls::{
    app::format_group_request, ds::WakuDeliveryService, mls_crypto::format_wallet_address,
    protos::de_mls::messages::v1::GroupUpdateRequest,
};
use de_mls_ui_protocol::v1::{AppEvent, MemberInfo};
use futures::channel::mpsc::UnboundedSender;
use hashgraph_like_consensus::events::ConsensusEventBus;

use crate::{CoreCtx, Gateway, UserRef};

/// Render a batch of approved proposals as `(action, identity)` pairs,
/// dropping any entry with an empty payload.
pub(crate) fn display_batch(batch: &[GroupUpdateRequest]) -> Vec<(String, String)> {
    batch
        .iter()
        .filter(|p| p.payload.is_some())
        .map(format_group_request)
        .collect()
}

/// Load the member roster for `group_name`, joining addresses with scores,
/// roles, and pending-leave markers into `MemberInfo` records.
pub(crate) async fn load_member_info(
    user: &UserRef,
    group_name: &str,
) -> anyhow::Result<Vec<MemberInfo>> {
    let user = user.read().await;
    let addresses = user.get_group_members(group_name).await?;
    let scores = user.get_member_scores(group_name).await;
    let roles = user.get_member_roles(group_name).await.unwrap_or_default();
    let pending_leavers: Vec<String> = user
        .get_pending_leave_identities(group_name)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|id| format_wallet_address(id.as_slice()).to_string())
        .collect();

    Ok(addresses
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
            let pending_leave = pending_leavers.iter().any(|a| a == &address);
            MemberInfo {
                address,
                score,
                role,
                pending_leave,
            }
        })
        .collect())
}

/// Push refreshed approved-queue and epoch-history events to the UI.
pub(crate) async fn push_consensus_state(
    user: &UserRef,
    evt_tx: &UnboundedSender<AppEvent>,
    group_name: &str,
) {
    if let Ok(proposals) = user
        .read()
        .await
        .get_approved_proposal_for_current_epoch(group_name)
        .await
    {
        let _ = evt_tx.unbounded_send(AppEvent::CurrentEpochProposals {
            group_id: group_name.to_string(),
            proposals: display_batch(&proposals),
        });
    }

    if let Ok(history) = user.read().await.get_epoch_history(group_name).await {
        let epochs: Vec<Vec<(String, String)>> =
            history.iter().map(|batch| display_batch(batch)).collect();
        let _ = evt_tx.unbounded_send(AppEvent::EpochHistory {
            group_id: group_name.to_string(),
            epochs,
        });
    }

    if let Ok((epoch, retry_round)) = user.read().await.get_epoch_and_retry(group_name).await {
        let _ = evt_tx.unbounded_send(AppEvent::GroupEpoch {
            group_id: group_name.to_string(),
            epoch,
            retry_round,
        });
    }
}

/// Push refreshed member scores and steward status to the UI.
///
/// Called after consensus events that may have changed peer scores
/// (emergency criteria proposals produce score ops on resolution).
pub(crate) async fn push_member_scores(
    user: &UserRef,
    evt_tx: &UnboundedSender<AppEvent>,
    group_name: &str,
) {
    let Ok(members) = load_member_info(user, group_name).await else {
        return;
    };
    let _ = evt_tx.unbounded_send(AppEvent::GroupMembers {
        group_id: group_name.to_string(),
        members,
    });

    let is_steward = user
        .read()
        .await
        .is_steward_for_group(group_name)
        .await
        .unwrap_or(false);
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
            tracing::info!("consensus forwarder started");
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
                    tracing::warn!(group = %group_name, error = %e, "apply_consensus_outcome failed");
                }

                // Push refreshed approved queue, epoch history, and member scores
                push_consensus_state(&user, &evt_tx, &group_name).await;
                push_member_scores(&user, &evt_tx, &group_name).await;
            }
            tracing::info!("consensus forwarder ended");
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
            tracing::info!("pubsub forwarder started");

            loop {
                let pkt = match rx.recv().await {
                    Ok(pkt) => pkt,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(dropped = n, "pubsub forwarder lagged");
                        continue;
                    }
                    Err(_) => break,
                };
                if !core.topics.contains(&pkt.group_id, &pkt.subtopic).await {
                    continue;
                }

                let group_id = pkt.group_id.clone();

                if let Err(e) = user.write().await.process_inbound_packet(pkt).await {
                    tracing::error!(group = %group_id, error = %e, "process_inbound_packet failed");
                }

                // Push refreshed approved queue + epoch history + members.
                push_consensus_state(&user, &evt_tx, &group_id).await;
                push_member_scores(&user, &evt_tx, &group_id).await;
            }

            tracing::info!("pubsub forwarder ended");
        });
    }
}
