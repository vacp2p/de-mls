//! Gateway-side fan-out from per-session [`SessionEvent`]s to the UI event pipe.
//!
//! [`crate::Gateway`] runs one polling task per logged-in user. Each tick
//! drains [`de_mls::app::User::drain_lifecycle_events`] for `Created` /
//! `Removed`, then drains [`de_mls::app::SessionRunner::drain_events`] on
//! every active session and dispatches the [`SessionEvent`]s to `AppEvent`
//! variants on the UI pipe — also maintaining the per-group
//! `epoch_history` cache used by the History tab.

use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;

use de_mls::{
    app::format_conversation_request,
    core::SessionEvent,
    ds::TopicFilter,
    protos::de_mls::messages::v1::{AppMessage, ConversationMessage, app_message},
};
use de_mls_ui_protocol::v1::AppEvent;

use crate::{EpochHistoryStore, MAX_EPOCH_HISTORY, forwarder::display_batch};

/// Fan-out target for [`SessionEvent`]s on a single conversation. Held as
/// `Arc` because the spawned per-session subscriber task owns a clone.
pub(crate) struct GatewaySessionFanout {
    pub evt_tx: UnboundedSender<AppEvent>,
    pub topics: Arc<TopicFilter>,
    pub epoch_history: EpochHistoryStore,
}

impl GatewaySessionFanout {
    /// Dispatch one [`SessionEvent`] to the UI pipe + side caches.
    pub(crate) async fn handle(&self, conversation_name: &str, event: SessionEvent) {
        match event {
            SessionEvent::AppMessage(message) => {
                let _ = forward_app_message(&self.evt_tx, message);
            }
            SessionEvent::Joined => {
                // No UI event today; topic subscription was already
                // arranged at conversation creation.
            }
            SessionEvent::Leaving => {
                if let Err(e) = self.topics.remove_many(conversation_name) {
                    tracing::warn!(error = %e, "topic filter remove failed");
                }
                self.epoch_history.lock().remove(conversation_name);
                let _ = self
                    .evt_tx
                    .unbounded_send(AppEvent::GroupRemoved(conversation_name.to_string()));
                let _ = self
                    .evt_tx
                    .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                        message: format!("You're removed from the group {conversation_name}")
                            .into_bytes(),
                        sender: "system".to_string(),
                        conversation_name: conversation_name.to_string(),
                    }));
            }
            SessionEvent::Error { operation, message } => {
                let _ = self.evt_tx.unbounded_send(AppEvent::Error(format!(
                    "{operation} failed for group {conversation_name}: {message}"
                )));
            }
            SessionEvent::OwnProposalSubmitted {
                proposal_id,
                request,
            } => {
                let (action, address) = format_conversation_request(&request);
                let _ = self.evt_tx.unbounded_send(AppEvent::OwnProposalSubmitted {
                    conversation_id: conversation_name.to_string(),
                    proposal_id,
                    action,
                    address,
                });
            }
            SessionEvent::PhaseChange(state) => {
                let _ = self.evt_tx.unbounded_send(AppEvent::GroupStateChanged {
                    conversation_id: conversation_name.to_string(),
                    state: state.to_string(),
                });
            }
            SessionEvent::CommitApplied(batch) => {
                if batch.is_empty() {
                    return;
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
            }
            SessionEvent::ProposalDecided(_) => {
                // Handled by the per-conv consensus forwarder
                // (see `Gateway::spawn_consensus_forwarder`) which pushes
                // `AppEvent::ProposalDecided` directly and also drives
                // `push_consensus_state` / `push_member_scores`.
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
        Some(app_message::Payload::ConversationMessage(cm)) => {
            let msg = cm.clone();
            evt_tx
                .unbounded_send(AppEvent::ChatMessage(msg))
                .map_err(|e| anyhow::anyhow!("error sending chat message event: {e}"))
        }
        // Other variants (BanRequest, KeyPackage, Proposal, Vote, CommitCandidate,
        // ConversationSync, ProposalAdded, UserVote) are protocol-internal —
        // not surfaced to the UI as chat-style messages.
        Some(_) => Ok(()),
        None => Err(anyhow::anyhow!("AppMessage payload missing")),
    }
}
