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
pub mod mls;
pub mod user;
mod welcome_envelope;

use std::{
    collections::{HashMap, VecDeque},
    str::FromStr,
    sync::{Arc, Mutex as StdMutex, atomic::AtomicBool},
};

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;

use de_mls::{
    ConversationConfig, ScoringConfig, StewardListConfig, defaults::DefaultConsensusPlugin,
    protos::de_mls::messages::v1::ConversationUpdateRequest,
};
use de_mls_ds::{DeliveryService, SharedDeliveryService, WakuDeliveryService};
use de_mls_ui_protocol::v1::{AppCmd, AppEvent};
use openmls_basic_credential::SignatureKeyPair;

use crate::mls::{DefaultConversationPluginsFactory, build_credential};
use crate::user::{ConsensusContext, ConversationEntry, User, UserPlugins};
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use tokio::sync::Mutex;

use crate::handler::GatewayEventFanout;

pub use crate::bootstrap::{
    AppState, Bootstrap, BootstrapConfig, BootstrapError, CoreCtx, bootstrap_core,
    bootstrap_core_from_env,
};

/// Type alias for the user reference stored in the gateway.
///
/// The MLS engine is the reference [`mls::GatewayMls`] — `OpenMlsService` over
/// `OpenMlsRustCrypto` — minted per conversation by
/// [`mls::DefaultConversationPluginsFactory`]. The MLS signing keypair is
/// [`SignatureKeyPair`], owned by `User` and threaded into every signing call.
pub(crate) type UserRef = Arc<
    tokio::sync::RwLock<
        User<DefaultConsensusPlugin, DefaultConversationPluginsFactory, SignatureKeyPair>,
    >,
>;

/// Type alias for a conversation registry entry obtained via
/// `User::lookup_entry`. Re-exports the sync-locked entry from `de_mls::session`.
pub(crate) type ConversationRef =
    ConversationEntry<DefaultConsensusPlugin, DefaultConversationPluginsFactory>;

// Global, process-wide gateway instance
pub static GATEWAY: Lazy<Gateway<WakuDeliveryService>> = Lazy::new(Gateway::new);

/// Helper to set the core context once during startup (called by ui_bridge).
pub fn init_core(core: Arc<CoreCtx<WakuDeliveryService>>) {
    GATEWAY.set_core(core);
}

/// Cap on the per-group rolling history of committed batches kept on the gateway.
pub(crate) const MAX_EPOCH_HISTORY: usize = 10;

/// Per-group rolling history of committed batches, populated by
/// the gateway's event fanout on `CommitApplied` and consumed by the History tab via
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
    // gateway's event fanout so a `CommitApplied` event can append.
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
            epoch_history: Arc::new(parking_lot::Mutex::new(HashMap::new())),
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

#[derive(Debug, Clone)]
pub struct WalletMemberId {
    bytes: Vec<u8>,
    display: String,
}

impl WalletMemberId {
    pub fn from_address(addr: Address) -> Self {
        Self {
            bytes: addr.as_slice().to_vec(),
            display: addr.to_checksum(None),
        }
    }

    pub fn member_id_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn member_id_display(&self) -> &str {
        &self.display
    }
}

fn build_user_from_private_key(
    private_key: &str,
    transport: SharedDeliveryService,
) -> anyhow::Result<User<DefaultConsensusPlugin, DefaultConversationPluginsFactory, SignatureKeyPair>>
{
    let eth_signer = PrivateKeySigner::from_str(private_key)
        .map_err(|e| anyhow::anyhow!("invalid private key: {e}"))?;
    let member_id = WalletMemberId::from_address(eth_signer.address());

    let (credential, mls_signer) = build_credential(member_id.member_id_bytes())?;
    let conversation_plugins =
        DefaultConversationPluginsFactory::new(credential, mls_signer.clone());

    let consensus_signer = EthereumConsensusSigner::new(eth_signer);
    let consensus = ConsensusContext::<DefaultConsensusPlugin>::new(consensus_signer);

    let plugins = UserPlugins {
        conversation_plugins,
        consensus,
        default_conversation_config: ConversationConfig::default(),
        default_scoring_config: ScoringConfig::default(),
        default_steward_list_config: StewardListConfig::default(),
    };

    Ok(User::new_with_plugins(
        member_id, mls_signer, plugins, transport,
    ))
}

// Login and forwarder setup is specific to the WakuDeliveryService gateway
impl Gateway<WakuDeliveryService> {
    /// Create the user engine with a private key.
    /// Returns a derived display name (e.g., address string).
    pub async fn login_with_private_key(&self, private_key: String) -> anyhow::Result<String> {
        let core = self.core();

        // Hand the Waku delivery service directly to `User` as its transport.
        // `WakuDeliveryService` implements `DeliveryService`. Wrap in a std
        // `Mutex` because the trait takes `&mut self`.
        let transport: SharedDeliveryService =
            Arc::new(StdMutex::new(core.app_state.delivery.clone()));
        let transport_for_subscribers = transport.clone();

        let user = build_user_from_private_key(&private_key, transport)?;

        let user_address = user.member_id_string();
        let user_ref: UserRef = Arc::new(tokio::sync::RwLock::new(user));

        *self.user.write() = Some(user_ref.clone());

        // Per-conversation subscribers: one task watches the User's
        // lifecycle channel; on each `Created(name)`, spawn a task that
        // subscribes to the new conversation's `ConversationEvent` stream and
        // forwards to the UI pipe; the consensus event forwarder is
        // spawned on the same trigger.
        self.spawn_conversation_subscribers(user_ref.clone(), transport_for_subscribers);

        self.spawn_delivery_service_forwarder(core.clone(), user_ref.clone());
        Ok(user_address)
    }

    /// Spawn the gateway's UI event pump. Once per polling cycle it
    /// drains [`crate::user::User::drain_lifecycle_events`] (to learn
    /// when new conversations appear or disappear) and
    /// [`de_mls::Conversation::drain_events`] on every active
    /// conversation (to forward UI-bound events). Replaces the previous
    /// broadcast-channel subscriber pattern.
    fn spawn_conversation_subscribers(&self, user: UserRef, transport: SharedDeliveryService) {
        let evt_tx = self.evt_tx.clone();
        let topics = self.core().topics.clone();
        let epoch_history = self.epoch_history.clone();
        let user_for_loop = user.clone();

        tokio::spawn(async move {
            let mut active_fanouts: HashMap<String, Arc<GatewayEventFanout>> = HashMap::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                // Drain user-level lifecycle events first so newly-created
                // conversations get their fanout registered before we look for
                // events on them.
                let lifecycle = user_for_loop.read().await.drain_lifecycle_events();
                let app_id_snapshot = user_for_loop.read().await.app_id().to_vec();
                for event in lifecycle {
                    match event {
                        crate::user::ConversationLifecycle::Created(name) => {
                            let fanout = Arc::new(GatewayEventFanout {
                                evt_tx: evt_tx.clone(),
                                topics: topics.clone(),
                                epoch_history: epoch_history.clone(),
                                transport: transport.clone(),
                                app_id: app_id_snapshot.clone(),
                                user: user_for_loop.clone(),
                            });
                            active_fanouts.insert(name, fanout);
                        }
                        crate::user::ConversationLifecycle::Removed(name) => {
                            active_fanouts.remove(&name);
                        }
                    }
                }

                // Drain each active conversation's pending events.
                for (name, fanout) in &active_fanouts {
                    let entry = match user_for_loop.read().await.lookup_entry(name) {
                        Ok(Some(s)) => s,
                        _ => continue,
                    };
                    let events = match entry.read() {
                        Ok(g) => g.drain_events(),
                        Err(_) => {
                            tracing::warn!(
                                conversation = %name,
                                "conversation drain skipped: lock poisoned"
                            );
                            continue;
                        }
                    };
                    for event in events {
                        fanout.handle(name, event).await;
                    }

                    // Publish any outbound the conversation buffered. The conversation is
                    // pull-only — it never sends. `User`-driven ops already
                    // flushed their own outbound; this catches packets produced
                    // by direct conversation calls in the polling / handler paths
                    // (commit candidates, auto-votes, …).
                    let outbound = match entry.read() {
                        Ok(g) => g.drain_outbound(),
                        Err(_) => Vec::new(),
                    };
                    if !outbound.is_empty()
                        && let Ok(mut t) = transport.lock()
                    {
                        for out in outbound {
                            if let Err(e) = t.publish(out.into()) {
                                tracing::warn!(
                                    conversation = %name,
                                    error = %e,
                                    "outbound publish failed"
                                );
                            }
                        }
                    }
                }
            }
        });
    }
}
