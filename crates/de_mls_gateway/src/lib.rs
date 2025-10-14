//! de_mls_gateway: a thin facade between UI (AppCmd/AppEvent) and the core runtime.
//!
//! Responsibilities:
//! - Own a single event pipe UI <- gateway (`AppEvent`)
//! - Provide a command entrypoint UI -> gateway (`send(AppCmd)`)
//! - Hold references to the core context (`CoreCtx`) and current user actor
//! - Offer small helper methods (login_with_private_key, etc.)

use de_mls::protos::messages::v1::app_message;
use de_mls::user_actor::{
    CreateGroupRequest, GetProposalsForStewardVotingRequest, LeaveGroupRequest, SendGroupMessage,
    StartStewardEpochRequest, StewardMessageRequest, UserVoteRequest,
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

use de_mls::CoreCtx; // re-exported by your core crate
use de_mls_ui_protocol::v1::{AppCmd, AppEvent, ChatMsg, VotePayload};

use kameo::actor::ActorRef;

// If you want to store the user actor here
use de_mls::user::{User, UserAction}; // adjust to your actual module path

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

        // Create user actor via core helper (you implement this inside your core)
        let (user_ref, user_address) =
            create_user_instance(private_key.clone(), core.app_state.clone()).await?;

        *self.user.write() = Some(user_ref.clone());

        self.spawn_forwarder_once(core, self.user.read().clone().unwrap());
        Ok(user_address)
    }

    /// Get a copy of the current user ref (if logged in).
    pub fn user(&self) -> Option<ActorRef<User>> {
        self.user.read().clone()
    }

    /// Spawn the pubsub forwarder once, after first successful login.
    fn spawn_forwarder_once(&self, core: Arc<CoreCtx>, user: ActorRef<User>) {
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
                        if let Some(app_message::Payload::VotingProposal(vp)) = &app_msg.payload {
                            let _ = evt_tx.unbounded_send(AppEvent::VoteRequested(VotePayload {
                                group_id: vp.group_name.clone(),
                                message: vp.payload.to_string(),
                                timeout_ms: now_ms() as u64,
                                proposal_id: vp.proposal_id.clone(),
                            }));
                            Ok::<(), anyhow::Error>(())
                        } else {
                            // generic chat fallback
                            let _ = evt_tx.unbounded_send(AppEvent::ChatMessage(ChatMsg {
                                id: uuid::Uuid::new_v4().to_string(),
                                group_id: content_topic.application_name.to_string(),
                                author: "system".to_string(),
                                body: app_msg.to_string(),
                                ts_ms: now_ms(),
                            }));
                            Ok::<(), anyhow::Error>(())
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
                            .unbounded_send(AppEvent::ChatMessage(ChatMsg {
                                id: uuid::Uuid::new_v4().to_string(),
                                group_id: group_name.clone(),
                                author: "system".to_string(),
                                body: format!("You're removed from the group {group_name}"),
                                ts_ms: now_ms(),
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
                                        message: vp.payload.to_string(),
                                        timeout_ms: now_ms() as u64,
                                        proposal_id: vp.proposal_id.clone(),
                                    },
                                ));
                            } else {
                                return Err(anyhow::anyhow!("Invalid app message: {app_msg}"));
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
