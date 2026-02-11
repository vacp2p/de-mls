use std::{env::VarError, num::ParseIntError, sync::Arc};
use tokio::sync::{broadcast, broadcast::Sender};
use tokio_util::sync::CancellationToken;
use tracing::info;

use de_mls::ds::{
    DeliveryService, DeliveryServiceError, InboundPacket, TopicFilter, WakuConfig,
    WakuDeliveryService, WakuStartResult,
};
use hashgraph_like_consensus::service::DefaultConsensusService;

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
    /// Enable discv5 peer discovery.
    pub discv5: bool,
    /// UDP port for discv5 (default 9000).
    pub discv5_udp_port: u16,
    /// Bootstrap ENR strings for discv5.
    pub discv5_bootstrap_enrs: Vec<String>,
}

pub struct Bootstrap<DS: DeliveryService> {
    pub core: Arc<CoreCtx<DS>>,
    /// Cancels the Waku→broadcast forwarder task
    pub cancel: CancellationToken,
}

pub async fn bootstrap_core(
    cfg: BootstrapConfig,
) -> Result<Bootstrap<WakuDeliveryService>, BootstrapError> {
    let WakuStartResult {
        service: delivery,
        enr: _local_enr,
    } = WakuDeliveryService::start(WakuConfig {
        node_port: cfg.node_port,
        discv5: cfg.discv5,
        discv5_udp_port: cfg.discv5_udp_port,
        discv5_bootstrap_enrs: cfg.discv5_bootstrap_enrs,
    })?;

    // Broadcast inbound packets inside the app
    let (pubsub_tx, _) = broadcast::channel::<InboundPacket>(100);

    // Subscribe before moving delivery into AppState.
    let rx = delivery.subscribe();

    let app_state = Arc::new(AppState {
        delivery,
        pubsub: pubsub_tx.clone(),
    });

    let core = Arc::new(CoreCtx::new(app_state.clone()));

    // Forward delivery-service packets into broadcast via a dedicated thread
    let forward_cancel = CancellationToken::new();
    {
        let forward_cancel = forward_cancel.clone();
        std::thread::Builder::new()
            .name("ds-forwarder".into())
            .spawn(move || {
                use std::time::Duration;
                info!("Forwarding delivery → broadcast started");
                loop {
                    if forward_cancel.is_cancelled() {
                        break;
                    }
                    match rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(pkt) => {
                            let _ = pubsub_tx.send(pkt);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
                info!("Forwarding delivery → broadcast stopped");
            })
            .map_err(|e| {
                BootstrapError::DeliveryServiceError(DeliveryServiceError::Other(anyhow::anyhow!(
                    e
                )))
            })?;
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

    let discv5_bootstrap_enrs = std::env::var("DISCV5_BOOTSTRAP_ENRS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    bootstrap_core(BootstrapConfig {
        node_port,
        discv5: true,
        discv5_udp_port: node_port.saturating_add(1000),
        discv5_bootstrap_enrs,
    })
    .await
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
