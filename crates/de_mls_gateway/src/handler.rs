//! Gateway-side fan-out from per-session [`SessionEvent`]s to the UI event pipe.
//!
//! [`crate::Gateway`] runs one polling task per logged-in user. Each tick
//! drains [`crate::user::User::drain_lifecycle_events`] for `Created` /
//! `Removed`, then drains [`de_mls::app::SessionRunner::drain_events`] on
//! every active session and dispatches the [`SessionEvent`]s to `AppEvent`
//! variants on the UI pipe — also maintaining the per-group
//! `epoch_history` cache used by the History tab.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures::channel::mpsc::UnboundedSender;
use prost::Message;

use de_mls::{
    core::SessionEvent,
    ds::{OutboundPacket, SharedDeliveryService, TopicFilter, WELCOME_SUBTOPIC},
    protos::de_mls::messages::v1::{AppMessage, ConversationMessage, VotePayload, app_message},
};
use de_mls_ui_protocol::v1::{AppEvent, format_conversation_request};
use hashgraph_like_consensus::types::ConsensusEvent;

use crate::{
    EpochHistoryStore, MAX_EPOCH_HISTORY, UserRef,
    forwarder::{display_batch, push_consensus_state, push_member_scores},
    welcome_envelope,
};

/// Fan-out target for [`SessionEvent`]s on a single conversation. Held as
/// `Arc` because the spawned per-session subscriber task owns a clone.
pub(crate) struct GatewaySessionFanout {
    pub evt_tx: UnboundedSender<AppEvent>,
    pub topics: Arc<TopicFilter>,
    pub epoch_history: EpochHistoryStore,
    /// Shared transport handle. The [`SessionEvent::WelcomeReady`] arm
    /// uses it to publish the envelope-wrapped welcome on
    /// [`WELCOME_SUBTOPIC`].
    pub transport: SharedDeliveryService,
    /// `app_id` of the local user — stamped on the outbound welcome
    /// packet so the steward's own gateway dedupes the echo.
    pub app_id: Vec<u8>,
    /// User handle. The `ConsensusReached` arm refreshes epoch state and
    /// member scores through it.
    pub user: UserRef,
}

impl GatewaySessionFanout {
    /// Dispatch one [`SessionEvent`] to the UI pipe + side caches.
    pub(crate) async fn handle(&self, conversation_id: &str, event: SessionEvent) {
        match event {
            SessionEvent::AppMessage(message) => {
                let _ = forward_app_message(&self.evt_tx, message);
            }
            SessionEvent::Leaving => {
                if let Err(e) = self.topics.remove_many(conversation_id) {
                    tracing::warn!(error = %e, "topic filter remove failed");
                }
                self.epoch_history.lock().remove(conversation_id);
                let _ = self
                    .evt_tx
                    .unbounded_send(AppEvent::GroupRemoved(conversation_id.to_string()));
                let _ = self
                    .evt_tx
                    .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                        message: format!("You're removed from the group {conversation_id}")
                            .into_bytes(),
                        sender: "system".to_string(),
                        conversation_id: conversation_id.to_string(),
                    }));
            }
            SessionEvent::Error { operation, message } => {
                let _ = self.evt_tx.unbounded_send(AppEvent::Error(format!(
                    "{operation} failed for group {conversation_id}: {message}"
                )));
            }
            SessionEvent::OwnProposalSubmitted {
                proposal_id,
                request,
            } => {
                let (action, address) = format_conversation_request(&request);
                let _ = self.evt_tx.unbounded_send(AppEvent::OwnProposalSubmitted {
                    conversation_id: conversation_id.to_string(),
                    proposal_id,
                    action,
                    address,
                });
            }
            SessionEvent::VoteRequested {
                proposal_id,
                request,
            } => {
                // The library carries only the proposal + decoded request; the
                // gateway stamps a UI timestamp and packs the wire `VotePayload`
                // the desktop UI's vote affordance consumes.
                let vp = VotePayload {
                    conversation_id: conversation_id.to_string(),
                    proposal_id,
                    payload: request.encode_to_vec(),
                    timestamp: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                };
                let _ = self.evt_tx.unbounded_send(AppEvent::VoteRequested(vp));
                let _ = self
                    .evt_tx
                    .unbounded_send(AppEvent::CurrentEpochProposalsCleared {
                        conversation_id: conversation_id.to_string(),
                    });
            }
            SessionEvent::PhaseChange(state) => {
                let _ = self.evt_tx.unbounded_send(AppEvent::GroupStateChanged {
                    conversation_id: conversation_id.to_string(),
                    state: state.to_string(),
                });
            }
            SessionEvent::CommitApplied(batch) => {
                if batch.is_empty() {
                    return;
                }
                let formatted: Vec<Vec<(String, String)>> = {
                    let mut store = self.epoch_history.lock();
                    let entry = store.entry(conversation_id.to_string()).or_default();
                    if entry.len() >= MAX_EPOCH_HISTORY {
                        entry.pop_front();
                    }
                    entry.push_back(batch);
                    entry.iter().map(|b| display_batch(b)).collect()
                };
                let _ = self.evt_tx.unbounded_send(AppEvent::EpochHistory {
                    conversation_id: conversation_id.to_string(),
                    epochs: formatted,
                });
            }
            SessionEvent::ConsensusReached {
                proposal_id,
                approved,
                timestamp,
            } => {
                let event = if approved {
                    ConsensusEvent::ConsensusReached {
                        proposal_id,
                        result: true,
                        timestamp,
                    }
                } else {
                    ConsensusEvent::ConsensusFailed {
                        proposal_id,
                        timestamp,
                    }
                };
                let _ = self.evt_tx.unbounded_send(AppEvent::ProposalDecided(
                    conversation_id.to_string(),
                    event,
                ));
                push_consensus_state(&self.user, &self.evt_tx, conversation_id).await;
                push_member_scores(&self.user, &self.evt_tx, conversation_id).await;
            }
            SessionEvent::WelcomeReady(welcome) => {
                let bytes = welcome.welcome_bytes.len();
                let sync_bytes = welcome.conversation_sync_bytes.len();
                let packet = OutboundPacket::new(
                    welcome_envelope::encode_welcome(welcome),
                    WELCOME_SUBTOPIC,
                    conversation_id,
                    &self.app_id,
                );
                match self.transport.lock() {
                    Ok(mut t) => {
                        if let Err(e) = t.publish(packet) {
                            tracing::error!(
                                conversation = %conversation_id,
                                error = %e,
                                "welcome publish failed"
                            );
                        } else {
                            tracing::info!(
                                conversation = %conversation_id,
                                welcome_bytes = bytes,
                                sync_bytes,
                                "welcome forwarded on welcome subtopic"
                            );
                        }
                    }
                    Err(_) => {
                        tracing::error!(
                            conversation = %conversation_id,
                            "welcome publish skipped: transport lock poisoned"
                        );
                    }
                }
            }
        }
    }
}

/// Dispatch an AppMessage to the appropriate AppEvent variant on the UI pipe.
pub fn forward_app_message(
    evt_tx: &UnboundedSender<AppEvent>,
    app_msg: AppMessage,
) -> anyhow::Result<()> {
    match &app_msg.payload {
        Some(app_message::Payload::ConversationMessage(cm)) => {
            let msg = cm.clone();
            evt_tx
                .unbounded_send(AppEvent::ChatMessage(msg))
                .map_err(|e| anyhow::anyhow!("error sending chat message event: {e}"))
        }
        // Other variants (BanRequest, KeyPackage, Proposal, Vote, CommitCandidate,
        // ConversationSync, ProposalAdded, UserVote) are protocol-internal —
        // not surfaced to the UI as chat-style messages. Vote requests arrive
        // as a dedicated `SessionEvent::VoteRequested`, not as an AppMessage.
        Some(_) => Ok(()),
        None => Err(anyhow::anyhow!("AppMessage payload missing")),
    }
}
