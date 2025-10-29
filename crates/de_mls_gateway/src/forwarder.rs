use kameo::actor::ActorRef;

use std::sync::{atomic::Ordering, Arc};
use tracing::info;

use de_mls::{
    message::MessageType,
    protos::de_mls::messages::v1::{app_message, ConversationMessage},
    user::{User, UserAction},
    user_actor::LeaveGroupRequest,
    user_app_instance::CoreCtx,
};
use de_mls_ui_protocol::v1::AppEvent;

use crate::Gateway;

impl Gateway {
    pub(crate) fn spawn_consensus_forwarder(&self, core: Arc<CoreCtx>) -> anyhow::Result<()> {
        let evt_tx = self.evt_tx.clone();
        let mut rx = core.consensus.subscribe_decisions();

        tokio::spawn(async move {
            tracing::info!("gateway: consensus forwarder started");
            while let Ok(res) = rx.recv().await {
                let _ = evt_tx.unbounded_send(AppEvent::ProposalDecided(res));
            }
            tracing::info!("gateway: consensus forwarder ended");
        });
        Ok(())
    }

    /// Spawn the pubsub forwarder once, after first successful login.
    pub(crate) fn spawn_waku_forwarder(&self, core: Arc<CoreCtx>, user: ActorRef<User>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }

        let evt_tx = self.evt_tx.clone();

        tokio::spawn(async move {
            let mut rx = core.app_state.pubsub.subscribe();
            tracing::info!("gateway: pubsub forwarder started");

            while let Ok(wmsg) = rx.recv().await {
                let content_topic = wmsg.content_topic.clone();

                // fast-topic filter
                if !core.topics.contains(&content_topic).await {
                    continue;
                }

                // hand over to user actor to decide action
                let action = match user.ask(wmsg).await {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!("user.ask failed: {e}");
                        continue;
                    }
                };

                // route the action
                let res = match action {
                    UserAction::SendToWaku(msg) => core
                        .app_state
                        .waku_node
                        .send(msg)
                        .await
                        .map_err(|e| anyhow::anyhow!("error sending waku message: {e}")),

                    UserAction::SendToApp(app_msg) => {
                        // voting
                        let res = match &app_msg.payload {
                            Some(app_message::Payload::VotePayload(vp)) => evt_tx
                                .unbounded_send(AppEvent::VoteRequested(vp.clone()))
                                .map_err(|e| {
                                    anyhow::anyhow!("error sending vote requested event: {e}")
                                })
                                .and_then(|_| {
                                    // Also clear current epoch proposals when voting starts
                                    evt_tx.unbounded_send(AppEvent::CurrentEpochProposalsCleared {
                                        group_id: vp.group_id.clone(),
                                    })
                                    .map_err(|e| {
                                        anyhow::anyhow!("error sending clear current epoch proposals event: {e}")
                                    })
                                }),
                            Some(app_message::Payload::ProposalAdded(pa)) => evt_tx
                                .unbounded_send(AppEvent::from(pa.clone()))
                                .map_err(|e| {
                                    anyhow::anyhow!("error sending proposal added event: {e}")
                                }),
                            Some(app_message::Payload::BanRequest(br)) => evt_tx
                                .unbounded_send(AppEvent::from(br.clone()))
                                .map_err(|e| {
                                    anyhow::anyhow!("error sending proposal added event (ban request): {e}")
                                }),
                            Some(app_message::Payload::ConversationMessage(cm)) => evt_tx
                                .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                                    message: cm.message.clone(),
                                    sender: cm.sender.clone(),
                                    group_name: cm.group_name.clone(),
                                }))
                                .map_err(|e| anyhow::anyhow!("error sending chat message: {e}")),
                            _ => {
                                AppEvent::Error(format!("Invalid app message: {:?}", app_msg.payload.unwrap().message_type()));
                                Ok::<(), anyhow::Error>(())
                            }
                        };
                        match res {
                            Ok(()) => Ok(()),
                            Err(e) => Err(anyhow::anyhow!("error sending app message: {e}")),
                        }
                    }

                    UserAction::LeaveGroup(group_name) => {
                        let _ = user
                            .ask(LeaveGroupRequest {
                                group_name: group_name.clone(),
                            })
                            .await
                            .map_err(|e| anyhow::anyhow!("error leaving group: {e}"));

                        core.topics.remove_many(&group_name).await;
                        info!("Leave group: {:?}", &group_name);

                        let _ = evt_tx
                            .unbounded_send(AppEvent::GroupRemoved(group_name.clone()))
                            .map_err(|e| anyhow::anyhow!("error sending group removed event: {e}"));

                        let _ = evt_tx
                            .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                                message: format!("You're removed from the group {group_name}")
                                    .into_bytes(),
                                sender: "system".to_string(),
                                group_name: group_name.clone(),
                            }))
                            .map_err(|e| anyhow::anyhow!("error sending chat message: {e}"));
                        Ok::<(), anyhow::Error>(())
                    }
                    UserAction::DoNothing => Ok(()),
                };

                if let Err(e) = res {
                    tracing::warn!("error handling waku action: {e:?}");
                }
            }

            tracing::info!("gateway: pubsub forwarder ended");
        });
    }
}
