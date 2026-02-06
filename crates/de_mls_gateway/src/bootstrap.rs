use ds::{topic_filter::TopicFilter, DeliveryServiceError};
use ds::{
    transport::{DeliveryService, InboundPacket},
    waku::{WakuConfig, WakuDeliveryService},
};
use hashgraph_like_consensus::service::DefaultConsensusService;
use std::env::VarError;
use std::num::ParseIntError;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio::sync::broadcast::Sender;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct AppState<DS: DeliveryService> {
    pub delivery: DS,
    pub pubsub: Sender<InboundPacket>,
}

#[derive(Clone)]
pub struct CoreCtx<DS: DeliveryService> {
    pub app_state: Arc<AppState<DS>>,
    pub topics: Arc<TopicFilter>,
    pub consensus: DefaultConsensusService,
}

impl<DS: DeliveryService> CoreCtx<DS> {
    pub fn new(app_state: Arc<AppState<DS>>) -> Self {
        Self {
            app_state,
            topics: Arc::new(TopicFilter::new()),
            consensus: DefaultConsensusService::new_with_max_sessions(10),
        }
    }
}

#[derive(Clone, Debug)]
pub struct BootstrapConfig {
    /// TCP/UDP port for the embedded Waku node
    pub node_port: u16,
    /// Peer multiaddrs as strings (parsed by the transport impl).
    pub peers: Vec<String>,
}

pub struct Bootstrap<DS: DeliveryService> {
    pub core: Arc<CoreCtx<DS>>,
    /// Cancels the Waku→broadcast forwarder task
    pub cancel: CancellationToken,
}

pub async fn bootstrap_core(
    cfg: BootstrapConfig,
) -> Result<Bootstrap<WakuDeliveryService>, BootstrapError> {
    let delivery = WakuDeliveryService::start(WakuConfig {
        node_port: cfg.node_port,
        peers: cfg.peers,
    })
    .await?;

    // Broadcast inbound packets inside the app
    let (pubsub_tx, _) = broadcast::channel::<InboundPacket>(100);

    // Subscribe before moving delivery into AppState.
    let mut rx = delivery.subscribe();

    let app_state = Arc::new(AppState {
        delivery,
        pubsub: pubsub_tx.clone(),
    });

    let core = Arc::new(CoreCtx::new(app_state.clone()));

    // Forward delivery-service packets into broadcast
    let forward_cancel = CancellationToken::new();
    {
        let forward_cancel = forward_cancel.clone();
        tokio::spawn(async move {
            info!("Forwarding delivery → broadcast started");
            loop {
                tokio::select! {
                    _ = forward_cancel.cancelled() => break,
                    res = rx.recv() => {
                        match res {
                            Ok(pkt) => { let _ = pubsub_tx.send(pkt); }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
            info!("Forwarding delivery → broadcast stopped");
        });
    }

    Ok(Bootstrap {
        core,
        cancel: forward_cancel,
    })
}

pub async fn bootstrap_core_from_env() -> Result<Bootstrap<WakuDeliveryService>, BootstrapError> {
    let node_port = std::env::var("NODE_PORT")
        .map_err(|e| BootstrapError::EnvVar("NODE_PORT", e))?
        .parse::<u16>()?;

    let peer_addresses =
        std::env::var("PEER_ADDRESSES").map_err(|e| BootstrapError::EnvVar("PEER_ADDRESSES", e))?;
    let peers = peer_addresses
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    bootstrap_core(BootstrapConfig { node_port, peers }).await
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error("Failed to read env var {0}: {1}")]
    EnvVar(&'static str, #[source] VarError),

    #[error("Failed to parse int: {0}")]
    ParseInt(#[from] ParseIntError),

    #[error(transparent)]
    DeliveryServiceError(#[from] DeliveryServiceError),
}
