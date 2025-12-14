// de_mls/src/bootstrap.rs
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use ds::{
    transport::{DeliveryService, InboundPacket},
    waku::{WakuConfig, WakuDeliveryService},
};

use crate::user_app_instance::{AppState, CoreCtx};

#[derive(Clone, Debug)]
pub struct BootstrapConfig {
    /// TCP/UDP port for the embedded Waku node
    pub node_port: u16,
    /// Peer multiaddrs as strings (parsed by the transport impl).
    pub peers: Vec<String>,
}

pub struct Bootstrap {
    pub core: Arc<CoreCtx>,
    /// Cancels the Waku→broadcast forwarder task
    pub cancel: CancellationToken,
}

/// Same wiring you previously did in `main.rs`, now reusable for server & desktop.
pub async fn bootstrap_core(cfg: BootstrapConfig) -> anyhow::Result<Bootstrap> {
    // Start delivery service (Waku-backed for now)
    let delivery = Arc::new(
        WakuDeliveryService::start(WakuConfig {
            node_port: cfg.node_port,
            peers: cfg.peers,
        })
        .await?,
    );

    // Broadcast inbound packets inside the app
    let (pubsub_tx, _) = broadcast::channel::<InboundPacket>(100);

    let app_state = Arc::new(AppState {
        delivery: delivery.clone(),
        pubsub: pubsub_tx.clone(),
    });

    let core = Arc::new(CoreCtx::new(app_state.clone()));

    // Forward delivery-service packets into broadcast
    let forward_cancel = CancellationToken::new();
    {
        let forward_cancel = forward_cancel.clone();
        let mut rx = delivery.subscribe();
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

/// Helper that exactly mirrors your current env usage:
/// - requires NODE_PORT
/// - requires PEER_ADDRESSES (comma-separated multiaddrs)
pub async fn bootstrap_core_from_env() -> anyhow::Result<Bootstrap> {
    use anyhow::Context;

    let node_port = std::env::var("NODE_PORT")
        .context("NODE_PORT is not set")?
        .parse::<u16>()
        .context("Failed to parse NODE_PORT")?;

    let peer_addresses = std::env::var("PEER_ADDRESSES").context("PEER_ADDRESSES is not set")?;
    let peers = peer_addresses
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

    bootstrap_core(BootstrapConfig { node_port, peers }).await
}
