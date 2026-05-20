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
    core::DefaultConsensusPlugin,
    ds::{DeliveryService, WakuDeliveryService},
    protos::de_mls::messages::v1::ConversationUpdateRequest,
};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use tokio::sync::Mutex;

use crate::handler::GatewaySessionFanout;

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
        User<DefaultConsensusPlugin, de_mls::app::DefaultConversationPluginsFactory>,
    >,
>;

/// Type alias for a per-conversation session reference obtained via
/// `User::lookup_entry`. Mirrors [`UserRef`] but at the session granularity.
pub(crate) type SessionRef = Arc<
    tokio::sync::RwLock<
        de_mls::app::SessionRunner<
            DefaultConsensusPlugin,
            de_mls::app::DefaultConversationPluginsFactory,
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
    Arc<parking_lot::Mutex<HashMap<String, VecDeque<Vec<ConversationUpdateRequest>>>>>;

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
    // gateway's ConversationEventHandler so `on_commit_applied` can append.
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

        // Hand the Waku delivery service directly to `User` as its transport.
        // `WakuDeliveryService` implements `de_mls::ds::DeliveryService`.
        let transport: Arc<dyn de_mls::ds::DeliveryService> =
            Arc::new(core.app_state.delivery.clone());

        let user = User::with_private_key(private_key.as_str(), transport)?;

        let user_address = user.identity_string();
        let user_ref: UserRef = Arc::new(tokio::sync::RwLock::new(user));

        *self.user.write() = Some(user_ref.clone());

        // Per-conversation subscribers: one task watches the User's
        // lifecycle channel; on each `Created(name)`, spawn a task that
        // subscribes to the new session's `SessionEvent` stream and
        // forwards to the UI pipe; the consensus event forwarder is
        // spawned on the same trigger.
        self.spawn_session_subscribers(user_ref.clone());

        self.spawn_delivery_service_forwarder(core.clone(), user_ref.clone());
        Ok(user_address)
    }

    /// Spawn the user-level subscriber that watches
    /// [`User::subscribe_conversations`] and, on each `Created(name)`,
    /// spawns:
    /// - a per-session `SessionEvent` subscriber (UI fan-out), and
    /// - the existing per-conv consensus event forwarder.
    fn spawn_session_subscribers(&self, user: UserRef) {
        let evt_tx = self.evt_tx.clone();
        let topics = self.core().topics.clone();
        let epoch_history = self.epoch_history.clone();
        let user_for_loop = user.clone();
        let gateway_for_consensus_spawn = user.clone();

        tokio::spawn(async move {
            let mut lifecycle_rx = {
                let u = user_for_loop.read().await;
                u.subscribe_conversations()
            };
            while let Ok(event) = lifecycle_rx.recv().await {
                match event {
                    de_mls::core::ConversationLifecycle::Created(name) => {
                        let fanout = Arc::new(GatewaySessionFanout {
                            evt_tx: evt_tx.clone(),
                            topics: topics.clone(),
                            epoch_history: epoch_history.clone(),
                        });
                        let Some(session_rx) = ({
                            let u = user_for_loop.read().await;
                            match u.lookup_entry(&name) {
                                Ok(opt) => opt,
                                Err(e) => {
                                    tracing::warn!(
                                        conversation = %name,
                                        error = %e,
                                        "lookup_entry failed in lifecycle::Created"
                                    );
                                    continue;
                                }
                            }
                        }) else {
                            tracing::warn!(
                                conversation = %name,
                                "lifecycle::Created fired but session missing in registry"
                            );
                            continue;
                        };
                        let name_for_sub = name.clone();
                        let mut session_rx_inner = session_rx.read().await.subscribe();
                        tokio::spawn(async move {
                            while let Ok(event) = session_rx_inner.recv().await {
                                fanout.handle(&name_for_sub, event).await;
                            }
                        });
                        // Spawn the per-conv consensus event forwarder
                        // (handles `AppEvent::ProposalDecided` + state pushes).
                        GATEWAY
                            .spawn_consensus_forwarder(gateway_for_consensus_spawn.clone(), name);
                    }
                    de_mls::core::ConversationLifecycle::Removed(_) => {
                        // Per-session subscribers exit naturally when the
                        // session's broadcast sender drops on entry removal.
                    }
                }
            }
        });
    }
}
