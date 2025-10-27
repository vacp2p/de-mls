//! de_mls_gateway: a thin facade between UI (AppCmd/AppEvent) and the core runtime.
//!
//! Responsibilities:
//! - Own a single event pipe UI <- gateway (`AppEvent`)
//! - Provide a command entrypoint UI -> gateway (`send(AppCmd)`)
//! - Hold references to the core context (`CoreCtx`) and current user actor
//! - Offer small helper methods (login_with_private_key, etc.)

use de_mls::protos::de_mls::messages::v1::app_message;
use de_mls::user_actor::{
    CreateGroupRequest, GetCurrentEpochProposalsRequest, GetProposalsForStewardVotingRequest,
    GetUserStatusRequest, LeaveGroupRequest, SendGroupMessage, StartStewardEpochRequest,
    StewardMessageRequest, UserVoteRequest,
};
use de_mls::user_app_instance::{create_user_instance, STEWARD_EPOCH};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use de_mls::protos::consensus::v1::VotePayload;
use de_mls::protos::de_mls::messages::v1::ConversationMessage;
use de_mls::user_app_instance::CoreCtx;
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};

use kameo::actor::ActorRef;

// If you want to store the user actor here
use de_mls::steward;
use de_mls::user::{User, UserAction}; // adjust to your actual module path
use hex;

pub struct Gateway {
    // UI events (gateway -> UI)
    evt_tx: UnboundedSender<AppEvent>,
    evt_rx: RwLock<UnboundedReceiver<AppEvent>>,

    // UI commands (UI -> gateway). ui_bridge registers the sender here.
    cmd_tx: RwLock<Option<UnboundedSender<AppCmd>>>,

    // Core singletons/handles:
    core: RwLock<Option<Arc<CoreCtx>>>, // set once during startup

    // Current logged-in user actor
    user: RwLock<Option<ActorRef<User>>>,
    started: AtomicBool,
}

impl Gateway {
    fn new() -> Self {
        let (evt_tx, evt_rx) = unbounded();
        Self {
            evt_tx,
            evt_rx: RwLock::new(evt_rx),
            cmd_tx: RwLock::new(None),
            core: RwLock::new(None),
            user: RwLock::new(None),
            started: AtomicBool::new(false),
        }
    }

    /// Called once by the bootstrap (ui_bridge) to provide the core context.
    pub fn set_core(&self, core: Arc<CoreCtx>) {
        *self.core.write() = Some(core);
    }

    pub fn core(&self) -> Arc<CoreCtx> {
        self.core
            .read()
            .as_ref()
            .expect("Gateway core not initialized")
            .clone()
    }

    /// ui_bridge registers its command sender so `send` can work.
    pub fn register_cmd_sink(&self, tx: UnboundedSender<AppCmd>) {
        *self.cmd_tx.write() = Some(tx);
    }

    /// Push an event to the UI.
    pub fn push_event(&self, evt: AppEvent) {
        let _ = self.evt_tx.unbounded_send(evt);
    }

    /// Await next event on the UI side.
    pub async fn next_event(&self) -> Option<AppEvent> {
        self.evt_rx.write().next().await
    }

    /// UI convenience: enqueue a command (UI -> gateway).
    pub async fn send(&self, cmd: AppCmd) -> anyhow::Result<()> {
        if let Some(tx) = self.cmd_tx.read().clone() {
            tx.unbounded_send(cmd)
                .map_err(|e| anyhow::anyhow!("send cmd failed: {e}"))
        } else {
            Err(anyhow::anyhow!("cmd sink not registered"))
        }
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        // You can boot your Waku node here for desktop if needed.
        Ok(())
    }

    // ─────────────────────────── High-level helpers ───────────────────────────

    /// Create the user actor with a private key (no group yet).
    /// Returns a derived display name (e.g., address string).
    pub async fn login_with_private_key(&self, private_key: String) -> anyhow::Result<String> {
        let core = self.core();
        let consensus_service = core.consensus.as_ref().clone();

        // Create user actor via core helper (you implement this inside your core)
        let (user_ref, user_address) = create_user_instance(
            private_key.clone(),
            core.app_state.clone(),
            &consensus_service,
        )
        .await?;

        *self.user.write() = Some(user_ref.clone());

        self.spawn_waku_forwarder(core.clone(), self.user.read().clone().unwrap());
        self.spawn_consensus_forwarder(core.clone())?;
        Ok(user_address)
    }

    /// Get a copy of the current user ref (if logged in).
    pub fn user(&self) -> Option<ActorRef<User>> {
        self.user.read().clone()
    }

    fn spawn_consensus_forwarder(&self, core: Arc<CoreCtx>) -> anyhow::Result<()> {
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
    fn spawn_waku_forwarder(&self, core: Arc<CoreCtx>, user: ActorRef<User>) {
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
                            Some(app_message::Payload::VotingProposal(vp)) => evt_tx
                                .unbounded_send(AppEvent::VoteRequested(VotePayload {
                                    group_id: vp.group_name.clone(),
                                    group_requests: vp.group_requests.clone(),
                                    timestamp: now_ms() as u64,
                                    proposal_id: vp.proposal_id.clone(),
                                }))
                                .map_err(|e| {
                                    anyhow::anyhow!("error sending vote requested event: {e}")
                                })
                                .and_then(|_| {
                                    // Also clear current epoch proposals when voting starts
                                    evt_tx.unbounded_send(AppEvent::CurrentEpochProposalsCleared {
                                        group_id: vp.group_name.clone(),
                                    })
                                    .map_err(|e| {
                                        anyhow::anyhow!("error sending clear current epoch proposals event: {e}")
                                    })
                                }),
                            Some(app_message::Payload::ProposalAdded(pa)) => evt_tx
                                .unbounded_send(AppEvent::ProposalAdded {
                                    group_id: pa.group_id.clone(),
                                    action: pa.action.clone(),
                                    address: pa.address.clone(),
                                })
                                .map_err(|e| {
                                    anyhow::anyhow!("error sending proposal added event: {e}")
                                }),
                            Some(app_message::Payload::BanRequest(br)) => evt_tx
                                .unbounded_send(AppEvent::ProposalAdded {
                                    group_id: br.group_name.clone(),
                                    action: "Remove Member".to_string(),
                                    address: br.user_to_ban.clone(),
                                })
                                .map_err(|e| {
                                    anyhow::anyhow!("error sending proposal added event (ban request): {e}")
                                }),
                            Some(app_message::Payload::ClearCurrentEpochProposals(ccp)) => evt_tx
                                .unbounded_send(AppEvent::CurrentEpochProposalsCleared {
                                    group_id: ccp.group_id.clone(),
                                })
                                .map_err(|e| {
                                    anyhow::anyhow!("error sending clear current epoch proposals event: {e}")
                                }),
                            Some(app_message::Payload::ConversationMessage(cm)) => evt_tx
                                .unbounded_send(AppEvent::ChatMessage(ConversationMessage {
                                    message: cm.message.clone(),
                                    sender: cm.sender.clone(),
                                    group_name: cm.group_name.clone(),
                                }))
                                .map_err(|e| anyhow::anyhow!("error sending chat message: {e}")),
                            _ => {
                                AppEvent::Error(format!("Invalid app message: {app_msg}"));
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

    pub async fn create_group(&self, group_name: String) -> anyhow::Result<()> {
        let core = self.core();
        let user = if let Some(user) = self.user() {
            user
        } else {
            return Err(anyhow::anyhow!("user not logged in"));
        };
        user.ask(CreateGroupRequest {
            group_name: group_name.clone(),
            is_creation: true,
        })
        .await?;
        core.topics.add_many(&group_name).await;
        core.groups.insert(group_name.clone()).await;
        info!("User start sending steward message for group {group_name:?}");
        let user_clone = user.clone();
        let group_name_clone = group_name.clone();
        let evt_tx_clone = self.evt_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(STEWARD_EPOCH));
            loop {
                interval.tick().await;
                // Step 1: Start steward epoch - check for proposals and start epoch if needed
                let proposals_count = user_clone
                    .ask(StartStewardEpochRequest {
                        group_name: group_name.clone(),
                    })
                    .await?;

                // Step 2: Send new steward key to the waku node for new epoch
                let msg = user_clone
                    .ask(StewardMessageRequest {
                        group_name: group_name.clone(),
                    })
                    .await?;
                core.app_state.waku_node.send(msg).await?;

                if proposals_count == 0 {
                    info!("No proposals to vote on for group: {group_name}, completing epoch without voting");
                } else {
                    info!("Found {proposals_count} proposals to vote on for group: {group_name}");

                    // Step 3: Start voting process - steward gets proposals for voting
                    let action = user_clone
                        .ask(GetProposalsForStewardVotingRequest {
                            group_name: group_name.clone(),
                        })
                        .await?;

                    // Step 4: Send proposals to ws to steward to vote or do nothing if no proposals
                    // After voting, steward sends vote and proposal to waku node and start consensus process
                    match action {
                        UserAction::SendToApp(app_msg) => {
                            if let Some(app_message::Payload::VotingProposal(vp)) = &app_msg.payload
                            {
                                info!("Sending app message to UI");
                                let _ = evt_tx_clone.unbounded_send(AppEvent::VoteRequested(
                                    VotePayload {
                                        group_id: group_name.clone(),
                                        group_requests: vp.group_requests.clone(),
                                        timestamp: now_ms() as u64,
                                        proposal_id: vp.proposal_id.clone(),
                                    },
                                ));

                                // Also clear current epoch proposals when voting starts
                                let _ = evt_tx_clone.unbounded_send(
                                    AppEvent::CurrentEpochProposalsCleared {
                                        group_id: group_name.clone(),
                                    },
                                );
                            }
                            if let Some(app_message::Payload::ProposalAdded(pa)) = &app_msg.payload
                            {
                                info!("Sending proposal added event to UI");
                                let _ = evt_tx_clone.unbounded_send(AppEvent::ProposalAdded {
                                    group_id: pa.group_id.clone(),
                                    action: pa.action.clone(),
                                    address: pa.address.clone(),
                                });
                            }
                            if let Some(app_message::Payload::ClearCurrentEpochProposals(ccp)) =
                                &app_msg.payload
                            {
                                info!("Sending clear current epoch proposals event to UI");
                                let _ = evt_tx_clone.unbounded_send(
                                    AppEvent::CurrentEpochProposalsCleared {
                                        group_id: ccp.group_id.clone(),
                                    },
                                );
                            }
                        }
                        UserAction::DoNothing => {
                            info!("No action to take for group: {group_name}");
                            return Ok(());
                        }
                        _ => {
                            return Err(anyhow::anyhow!("Invalid user action: {action}"));
                        }
                    }
                }
            }
        });
        tracing::debug!("User started sending steward message for group {group_name_clone:?}");
        Ok(())
    }

    pub async fn join_group(&self, group_name: String) -> anyhow::Result<()> {
        let core = self.core();
        let user = if let Some(user) = self.user() {
            user
        } else {
            return Err(anyhow::anyhow!("user not logged in"));
        };
        user.ask(CreateGroupRequest {
            group_name: group_name.clone(),
            is_creation: false,
        })
        .await?;
        core.topics.add_many(&group_name).await;
        core.groups.insert(group_name.clone()).await;
        tracing::debug!("User joined group {group_name}");
        tracing::debug!(
            "User have topic for group {:?}",
            core.topics.snapshot().await
        );
        Ok(())
    }

    pub async fn send_message(&self, group_name: String, message: String) -> anyhow::Result<()> {
        let core = self.core();
        let user = if let Some(user) = self.user() {
            user
        } else {
            return Err(anyhow::anyhow!("user not logged in"));
        };
        let pmt = user
            .ask(SendGroupMessage {
                message: message.clone().into_bytes(),
                group_name: group_name.clone(),
            })
            .await?;
        core.app_state.waku_node.send(pmt).await?;
        Ok(())
    }

    pub async fn send_ban_request(
        &self,
        group_name: String,
        user_to_ban: String,
    ) -> anyhow::Result<()> {
        let core = self.core();
        let user = if let Some(user) = self.user() {
            user
        } else {
            return Err(anyhow::anyhow!("user not logged in"));
        };

        let ban_request = de_mls::protos::de_mls::messages::v1::BanRequest {
            user_to_ban: user_to_ban.clone(),
            requester: String::new(),
            group_name: group_name.clone(),
        };

        let msg = user
            .ask(de_mls::user_actor::BuildBanMessage {
                ban_request,
                group_name: group_name.clone(),
            })
            .await?;
        match msg {
            UserAction::SendToWaku(msg) => {
                core.app_state.waku_node.send(msg).await?;
            }
            UserAction::SendToApp(msg) => {
                self.push_event(AppEvent::ProposalAdded {
                    group_id: group_name.clone(),
                    action: "Remove Member".to_string(),
                    address: user_to_ban.clone(),
                });
            }
            _ => return Err(anyhow::anyhow!("Invalid user action")),
        }

        Ok(())
    }

    pub async fn process_user_vote(
        &self,
        group_name: String,
        proposal_id: u32,
        vote: bool,
    ) -> anyhow::Result<()> {
        let user = if let Some(user) = self.user() {
            user
        } else {
            return Err(anyhow::anyhow!("user not logged in"));
        };

        let user_vote_result = user
            .ask(UserVoteRequest {
                group_name: group_name.clone(),
                proposal_id,
                vote,
            })
            .await?;
        if let Some(waku_msg) = user_vote_result {
            self.core().app_state.waku_node.send(waku_msg).await?;
        }
        Ok(())
    }

    pub async fn group_list(&self) -> anyhow::Result<Vec<String>> {
        let core = self.core();
        let groups = core.groups.all().await;
        Ok(groups)
    }

    pub async fn query_steward(&self, group_name: String) -> anyhow::Result<bool> {
        let user = self
            .user()
            .ok_or_else(|| anyhow::anyhow!("user not logged in"))?;
        let is_steward = user.ask(GetUserStatusRequest { group_name }).await?;
        Ok(is_steward)
    }

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        group_name: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let user = self
            .user()
            .ok_or_else(|| anyhow::anyhow!("user not logged in"))?;

        let proposals = user
            .ask(GetCurrentEpochProposalsRequest { group_name })
            .await?;
        let display_proposals: Vec<(String, String)> = proposals
            .iter()
            .map(|proposal| match proposal {
                steward::GroupUpdateRequest::AddMember(kp) => {
                    let address = format!(
                        "0x{}",
                        hex::encode(kp.leaf_node().credential().serialized_content())
                    );
                    ("Add Member".to_string(), address)
                }
                steward::GroupUpdateRequest::RemoveMember(id) => {
                    ("Remove Member".to_string(), id.clone())
                }
            })
            .collect();
        Ok(display_proposals)
    }
}

// Global, process-wide gateway instance
pub static GATEWAY: Lazy<Gateway> = Lazy::new(Gateway::new);

/// Helper to set the core context once during startup (called by ui_bridge).
pub fn init_core(core: Arc<CoreCtx>) {
    GATEWAY.set_core(core);
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
