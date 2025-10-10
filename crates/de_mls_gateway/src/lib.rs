//! de_mls_gateway: a thin façade between UI (AppCmd/AppEvent) and the core runtime.
//!
//! Responsibilities:
//! - Own a single event pipe UI <- gateway (`AppEvent`)
//! - Provide a command entrypoint UI -> gateway (`send(AppCmd)`)
//! - Hold references to the core context (`CoreCtx`) and current user actor
//! - Offer small helper methods (login_with_private_key, etc.)

use de_mls::user_actor::CreateGroupRequest;
use de_mls::user_app_instance::{
    create_user_instance_only, handle_steward_flow_per_epoch, STEWARD_EPOCH,
};
use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};

use de_mls::CoreCtx; // re-exported by your core crate
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};

use kameo::actor::ActorRef;

// If you want to store the user actor here
use de_mls::user::User; // adjust to your actual module path

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

    /// Start any background pieces the desktop UI expects.
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
            create_user_instance_only(private_key.clone(), core.app_state.clone()).await?;

        *self.user.write() = Some(user_ref);
        Ok(user_address)
    }

    /// Get a copy of the current user ref (if logged in).
    pub fn user(&self) -> Option<ActorRef<User>> {
        self.user.read().clone()
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
        info!("User start sending steward message for group {group_name:?}");
        let user_clone = user.clone();
        let group_name_clone = group_name.clone();
        let app_state_steward = core.app_state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(STEWARD_EPOCH));
            loop {
                interval.tick().await;
                let _ = async {
                    handle_steward_flow_per_epoch(
                        user_clone.clone(),
                        group_name_clone.clone(),
                        app_state_steward.clone(),
                    )
                    .await
                }
                .await
                .inspect_err(|e| error!("Error sending steward message to waku: {e}"));
            }
        });
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
