use std::sync::{Arc, atomic::Ordering};

use de_mls::{protos::de_mls::messages::v1::ConversationUpdateRequest, session::SessionError};
use de_mls_ds::WakuDeliveryService;

use crate::user::Inbound;
use de_mls_ui_protocol::v1::{AppEvent, MemberInfo, encode_hex, format_conversation_request};
use futures::channel::mpsc::UnboundedSender;

use crate::{CoreCtx, Gateway, SessionRef, UserRef};

/// Look up a per-conversation session from the `User` registry. Returns
/// `Err(ConversationNotFound)` when the conversation has been removed.
/// Centralized so every call site uses the same lookup + error shape.
pub(crate) async fn lookup_session(
    user: &UserRef,
    conversation_id: &str,
) -> Result<SessionRef, SessionError> {
    user.read()
        .await
        .lookup_entry(conversation_id)?
        .ok_or(SessionError::ConversationNotFound)
}

/// Render a batch of approved proposals as `(action, member_id)` pairs,
/// dropping any entry with an empty payload.
pub(crate) fn display_batch(batch: &[ConversationUpdateRequest]) -> Vec<(String, String)> {
    batch
        .iter()
        .filter(|p| p.payload.is_some())
        .map(format_conversation_request)
        .collect()
}

/// Load the member roster for `conversation_id`, joining addresses with scores,
/// roles, and pending-leave markers into `MemberInfo` records.
pub(crate) async fn load_member_info(
    user: &UserRef,
    conversation_id: &str,
) -> anyhow::Result<Vec<MemberInfo>> {
    let session = lookup_session(user, conversation_id).await?;
    let runner = session
        .read()
        .map_err(|_| SessionError::LockPoisoned("session"))?;
    let member_bytes = runner.get_conversation_members()?;
    let scores = runner.get_member_scores();
    let roles = runner.get_member_roles().unwrap_or_default();
    let pending_leavers = runner.get_pending_leave_member_ids().unwrap_or_default();

    Ok(member_bytes
        .into_iter()
        .map(|id| {
            let score = scores
                .iter()
                .find(|(raw_id, _)| raw_id == &id)
                .map(|(_, s)| *s)
                .unwrap_or(100);
            let role = roles
                .iter()
                .find(|(raw_id, _)| raw_id == &id)
                .map(|(_, r)| r.to_string())
                .unwrap_or_else(|| "member".to_string());
            let pending_leave = pending_leavers.iter().any(|p| p == &id);
            MemberInfo {
                address: encode_hex(&id),
                score,
                role,
                pending_leave,
            }
        })
        .collect())
}

/// Push refreshed approved-queue and current-epoch state to the UI.
pub(crate) async fn push_consensus_state(
    user: &UserRef,
    evt_tx: &UnboundedSender<AppEvent>,
    conversation_id: &str,
) {
    let Ok(session) = lookup_session(user, conversation_id).await else {
        return;
    };
    let runner = match session.read() {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!(
                conversation = %conversation_id,
                "push_consensus_state skipped: session lock poisoned"
            );
            return;
        }
    };
    let proposals = runner.get_approved_proposals_for_current_epoch();
    let _ = evt_tx.unbounded_send(AppEvent::CurrentEpochProposals {
        conversation_id: conversation_id.to_string(),
        proposals: display_batch(&proposals),
    });

    if let Ok((epoch, retry_round)) = runner.get_epoch_and_retry() {
        let _ = evt_tx.unbounded_send(AppEvent::GroupEpoch {
            conversation_id: conversation_id.to_string(),
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
    conversation_id: &str,
) {
    let Ok(members) = load_member_info(user, conversation_id).await else {
        return;
    };
    let _ = evt_tx.unbounded_send(AppEvent::GroupMembers {
        conversation_id: conversation_id.to_string(),
        members,
    });

    let Ok(session) = lookup_session(user, conversation_id).await else {
        return;
    };
    let is_steward = match session.read() {
        Ok(s) => s.is_steward_for_self(),
        Err(_) => {
            tracing::warn!(
                conversation = %conversation_id,
                "is_steward read skipped: session lock poisoned"
            );
            return;
        }
    };
    let _ = evt_tx.unbounded_send(AppEvent::StewardStatus {
        conversation_id: conversation_id.to_string(),
        is_steward,
    });
}

impl Gateway<WakuDeliveryService> {
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
                match core.topics.contains(&pkt.conversation_id, &pkt.subtopic) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, "topic filter contains check failed");
                        continue;
                    }
                }

                let conversation_id = pkt.conversation_id.clone();
                let is_welcome_channel = pkt.subtopic == de_mls_ds::WELCOME_SUBTOPIC;

                if is_welcome_channel
                    && let Some(mw) = crate::welcome_envelope::decode(&pkt.payload)
                {
                    let accepted = user.write().await.accept_welcome(&mw.welcome_bytes);
                    match accepted {
                        Ok(_) if !mw.conversation_sync_bytes.is_empty() => {
                            // Replay the bundled ConversationSync through the
                            // standard inbound path now that MLS is attached —
                            // the sync payload is an MLS-encrypted app message
                            // addressed to the new epoch.
                            let sync_inbound = Inbound {
                                conversation_id: pkt.conversation_id.clone(),
                                sender: pkt.app_id.clone(),
                                payload: mw.conversation_sync_bytes,
                            };
                            if let Err(e) = user.read().await.handle_inbound(sync_inbound) {
                                tracing::warn!(
                                    group = %conversation_id,
                                    error = %e,
                                    "bundled sync replay failed"
                                );
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                group = %conversation_id,
                                error = %e,
                                "accept_welcome failed"
                            );
                        }
                    }
                    continue;
                }

                // Route by the integrator's own channel knowledge: the welcome
                // channel carries a joiner's key-package announcement; every
                // other channel carries conversation traffic.
                let inbound = Inbound {
                    conversation_id: pkt.conversation_id.clone(),
                    sender: pkt.app_id,
                    payload: pkt.payload,
                };
                let result = if is_welcome_channel {
                    user.read().await.receive_key_package(inbound)
                } else {
                    user.read().await.handle_inbound(inbound)
                };
                if let Err(e) = result {
                    tracing::error!(group = %conversation_id, error = %e, "inbound handling failed");
                }

                // Push refreshed approved queue + epoch history + members.
                push_consensus_state(&user, &evt_tx, &conversation_id).await;
                push_member_scores(&user, &evt_tx, &conversation_id).await;
            }

            tracing::info!("pubsub forwarder ended");
        });
    }
}
