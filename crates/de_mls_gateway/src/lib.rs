//! de_mls_gateway: a thin facade between UI (AppCmd/AppEvent) and the core runtime.
//!
//! Responsibilities:
//! - Own a single event pipe UI <- gateway (`AppEvent`)
//! - Provide a command entrypoint UI -> gateway (`send(AppCmd)`)
//! - Hold references to the core context (`CoreCtx`) and current user
//! - Offer small helper methods (login_with_private_key, etc.)

mod bootstrap;
pub(crate) mod forwarder;
mod group;
pub mod handler;

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, atomic::AtomicBool},
};

use de_mls::{
    app::User,
    core::DefaultProvider,
    ds::{DeliveryService, WakuDeliveryService},
    protos::de_mls::messages::v1::GroupUpdateRequest,
};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use tokio::sync::Mutex;

use crate::handler::GatewayEventHandler;

pub use crate::bootstrap::{
    AppState, Bootstrap, BootstrapConfig, BootstrapError, CoreCtx, bootstrap_core,
    bootstrap_core_from_env,
};

/// Type alias for the user reference stored in the gateway.
///
/// Uses [`de_mls::app::DefaultMlsService`] — `OpenMlsService` over
/// `Arc<MemoryDeMlsStorage>` — so per-group services share one storage
/// (the `Arc<S>: DeMlsStorage` blanket impl makes this work). MLS
/// credentials live on `User` and are passed in at service construction.
type UserRef = Arc<
    tokio::sync::RwLock<
        User<
            DefaultProvider,
            de_mls::app::DefaultMlsService,
            de_mls::app::DefaultPeerScoring,
            de_mls::app::DefaultStewardList,
            de_mls::identity::WalletIdentity,
            GatewayEventHandler<WakuDeliveryService>,
            GatewayEventHandler<WakuDeliveryService>,
        >,
    >,
>;

// Global, process-wide gateway instance
pub static GATEWAY: Lazy<Gateway<WakuDeliveryService>> = Lazy::new(Gateway::new);

/// Helper to set the core context once during startup (called by ui_bridge).
pub fn init_core(core: Arc<CoreCtx<WakuDeliveryService>>) {
    GATEWAY.set_core(core);
}

/// Cap on the per-group rolling history of committed batches kept on the gateway.
pub(crate) const MAX_EPOCH_HISTORY: usize = 10;

/// Per-group rolling history of committed batches, populated by
/// `on_commit_applied` and consumed by the History tab via
/// `Gateway::get_epoch_history`. Cap is [`MAX_EPOCH_HISTORY`].
pub(crate) type EpochHistoryStore =
    Arc<parking_lot::Mutex<HashMap<String, VecDeque<Vec<GroupUpdateRequest>>>>>;

pub struct Gateway<DS: DeliveryService> {
    // UI events (gateway -> UI)
    evt_tx: UnboundedSender<AppEvent>,
    evt_rx: Mutex<UnboundedReceiver<AppEvent>>,

    // UI commands (UI -> gateway)
    cmd_tx: RwLock<Option<UnboundedSender<AppCmd>>>,

    // Core context (set once during startup)
    core: RwLock<Option<Arc<CoreCtx<DS>>>>,

    // Current logged-in user
    user: RwLock<Option<UserRef>>,

    // Guards against spawning forwarders more than once
    started: AtomicBool,

    // Per-group committed-batch history (UI cache). Shared by Arc with the
    // gateway's GroupEventHandler so `on_commit_applied` can append.
    epoch_history: EpochHistoryStore,
}

impl<DS: DeliveryService> Gateway<DS> {
    fn new() -> Self {
        let (evt_tx, evt_rx) = unbounded();
        Self {
            evt_tx,
            evt_rx: Mutex::new(evt_rx),
            cmd_tx: RwLock::new(None),
            core: RwLock::new(None),
            user: RwLock::new(None),
            started: AtomicBool::new(false),
            epoch_history: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Called once by the bootstrap (ui_bridge) to provide the core context.
    pub fn set_core(&self, core: Arc<CoreCtx<DS>>) {
        *self.core.write() = Some(core);
    }

    pub fn core(&self) -> Arc<CoreCtx<DS>> {
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
        let mut rx = self.evt_rx.lock().await;
        rx.next().await
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

    // ─────────────────────────── High-level helpers ───────────────────────────

    /// Get a copy of the current user ref (if logged in).
    pub fn user(&self) -> anyhow::Result<UserRef> {
        self.user
            .read()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("user not logged in"))
    }
}

// Login and forwarder setup is specific to the WakuDeliveryService gateway
impl Gateway<WakuDeliveryService> {
    /// Create the user engine with a private key.
    /// Returns a derived display name (e.g., address string).
    pub async fn login_with_private_key(&self, private_key: String) -> anyhow::Result<String> {
        let core = self.core();
        let consensus_service = core.consensus.clone();

        // Create handler that implements both GroupEventHandler and StateChangeHandler
        let handler = Arc::new(GatewayEventHandler {
            delivery: Arc::new(core.app_state.delivery.clone()),
            evt_tx: self.evt_tx.clone(),
            topics: core.topics.clone(),
            epoch_history: self.epoch_history.clone(),
        });

        let user = User::with_private_key(
            private_key.as_str(),
            Arc::new(consensus_service),
            handler.clone(),
            handler, // Same handler implements StateChangeHandler
        )?;

        let user_address = user.identity_string();
        let user_ref: UserRef = Arc::new(tokio::sync::RwLock::new(user));

        *self.user.write() = Some(user_ref.clone());

        self.spawn_delivery_service_forwarder(core.clone(), user_ref.clone());
        self.spawn_consensus_forwarder(core.clone(), user_ref.clone());
        Ok(user_address)
    }
}
