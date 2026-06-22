//! Gateway-side fan-out from per-conversation [`ConversationEvent`]s to the UI event pipe.
//!
//! [`crate::Gateway`] runs one polling task per logged-in user. Each tick
//! drains [`crate::user::User::drain_lifecycle_events`] for `Created` /
//! `Removed`, then drains [`de_mls::Conversation::drain_events`] on
//! every active conversation and dispatches the [`ConversationEvent`]s to `AppEvent`
//! variants on the UI pipe — also maintaining the per-group
//! `epoch_history` cache used by the History tab.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use futures::channel::mpsc::UnboundedSender;
use prost::Message;

use de_mls::{
    ConversationEvent,
    protos::de_mls::messages::v1::{AppMessage, ConversationMessage, VotePayload, app_message},
};
use de_mls_ds::{OutboundPacket, SharedDeliveryService, TopicFilter, WELCOME_SUBTOPIC};
use de_mls_ui_protocol::v1::{AppEvent, format_conversation_request};
use hashgraph_like_consensus::types::ConsensusEvent;

use crate::{
    EpochHistoryStore, MAX_EPOCH_HISTORY, UserRef,
    forwarder::{display_batch, push_consensus_state, push_member_scores},
    render_member_id, welcome_envelope,
};

/// Fan-out target for [`ConversationEvent`]s on a single conversation. Held as
/// `Arc` because the spawned per-conversation subscriber task owns a clone.
pub(crate) struct GatewayEventFanout {
    pub evt_tx: UnboundedSender<AppEvent>,
    pub topics: Arc<TopicFilter>,
    pub epoch_history: EpochHistoryStore,
    /// Shared transport handle. The [`ConversationEvent::WelcomeReady`] arm
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

impl GatewayEventFanout {
    /// Dispatch one [`ConversationEvent`] to the UI pipe + side caches.
    pub(crate) async fn handle(&self, conversation_id: &str, event: ConversationEvent) {
        match event {
            ConversationEvent::AppMessage(message) => {
                let _ = forward_app_message(&self.evt_tx, message);
            }
            ConversationEvent::Leaving => {
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
                        sender: b"system".to_vec(),
                        conversation_id: conversation_id.to_string(),
                        ..Default::default()
                    }));
            }
            ConversationEvent::Error { operation, message } => {
                let _ = self.evt_tx.unbounded_send(AppEvent::Error(format!(
                    "{operation} failed for group {conversation_id}: {message}"
                )));
            }
            ConversationEvent::OwnProposalSubmitted {
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
            ConversationEvent::VoteRequested {
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
            ConversationEvent::PhaseChange(state) => {
                let _ = self.evt_tx.unbounded_send(AppEvent::GroupStateChanged {
                    conversation_id: conversation_id.to_string(),
                    state: state.to_string(),
                });
            }
            ConversationEvent::CommitApplied(batch) => {
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
            ConversationEvent::ConsensusReached {
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
            ConversationEvent::FreezeProgress { received, expected } => {
                let _ = self.evt_tx.unbounded_send(AppEvent::FreezeCandidates {
                    conversation_id: conversation_id.to_string(),
                    received,
                    expected,
                });
            }
            ConversationEvent::WelcomeReady {
                welcome,
                minted_locally,
            } => {
                // Only the minting committer publishes to the welcome
                // subtopic; peers receiving the in-group broadcast would
                // otherwise flood the joiner with duplicates.
                if !minted_locally {
                    return;
                }
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
            // The protocol carries the sender as opaque member-id bytes;
            // render them to the gateway's display form before handing the
            // message to the UI.
            let msg = ConversationMessage {
                message: cm.message.clone(),
                sender: render_member_id(&cm.sender).into_bytes(),
                conversation_id: cm.conversation_id.clone(),
                sender_credential: cm.sender_credential.clone(),
            };
            evt_tx
                .unbounded_send(AppEvent::ChatMessage(msg))
                .map_err(|e| anyhow::anyhow!("error sending chat message event: {e}"))
        }
        // Other variants (BanRequest, KeyPackage, Proposal, Vote, CommitCandidate,
        // ConversationSync, ProposalAdded, UserVote) are protocol-internal —
        // not surfaced to the UI as chat-style messages. Vote requests arrive
        // as a dedicated `ConversationEvent::VoteRequested`, not as an AppMessage.
        Some(_) => Ok(()),
        None => Err(anyhow::anyhow!("AppMessage payload missing")),
    }
}
