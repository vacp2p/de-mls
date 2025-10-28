// de_mls/src/bootstrap.rs
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use waku_bindings::{Multiaddr, WakuMessage};

use ds::waku_actor::{run_waku_node, WakuMessageToSend};

use crate::user_app_instance::{AppState, CoreCtx};

#[derive(Clone, Debug)]
pub struct BootstrapConfig {
    /// TCP/UDP port for the embedded Waku node
    pub node_port: String,
    /// Comma-separated peer multiaddrs parsed into a vec
    pub peers: Vec<Multiaddr>,
}

pub struct Bootstrap {
    pub core: Arc<CoreCtx>,
    /// Cancels the Waku→broadcast forwarder task
    pub cancel: CancellationToken,
    /// The thread running the Waku node runtime; join on shutdown if you want
    pub waku_thread: std::thread::JoinHandle<()>,
}

/// Same wiring you previously did in `main.rs`, now reusable for server & desktop.
pub async fn bootstrap_core(cfg: BootstrapConfig) -> anyhow::Result<Bootstrap> {
    // Channels used by AppState and Waku runtime
    let (waku_in_tx, mut waku_in_rx) = mpsc::channel::<WakuMessage>(100);
    let (to_waku_tx, mut to_waku_rx) = mpsc::channel::<WakuMessageToSend>(100);
    let (pubsub_tx, _) = broadcast::channel::<WakuMessage>(100);

    let app_state = Arc::new(AppState {
        waku_node: to_waku_tx.clone(),
        pubsub: pubsub_tx.clone(),
    });

    let core = Arc::new(CoreCtx::new(app_state.clone()));

    // Forward Waku messages into broadcast
    let forward_cancel = CancellationToken::new();
    {
        let forward_cancel = forward_cancel.clone();
        tokio::spawn(async move {
            info!("Forwarding Waku → broadcast started");
            loop {
                tokio::select! {
                    _ = forward_cancel.cancelled() => break,
                    maybe = waku_in_rx.recv() => {
                        if let Some(msg) = maybe {
                            let _ = pubsub_tx.send(msg);
                        } else {
                            break;
                        }
                    }
                }
            }
            info!("Forwarding Waku → broadcast stopped");
        });
    }

    // Start Waku node on a dedicated thread with its own Tokio runtime
    let node_port = cfg.node_port.clone();
    let peers = cfg.peers.clone();
    let waku_thread = std::thread::Builder::new()
        .name("waku-node".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("waku tokio runtime");
            rt.block_on(async move {
                if let Err(e) =
                    run_waku_node(node_port, Some(peers), waku_in_tx, &mut to_waku_rx).await
                {
                    error!("run_waku_node failed: {e}");
                }
            });
        })?;

    Ok(Bootstrap {
        core,
        cancel: forward_cancel,
        waku_thread,
    })
}

/// Helper that exactly mirrors your current env usage:
/// - requires NODE_PORT
/// - requires PEER_ADDRESSES (comma-separated multiaddrs)
pub async fn bootstrap_core_from_env() -> anyhow::Result<Bootstrap> {
    use anyhow::Context;

    let node_port = std::env::var("NODE_PORT").context("NODE_PORT is not set")?;
    let peer_addresses = std::env::var("PEER_ADDRESSES").context("PEER_ADDRESSES is not set")?;
    let peers = peer_addresses
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            s.parse::<Multiaddr>()
                .context(format!("Failed to parse peer address: {s}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    bootstrap_core(BootstrapConfig { node_port, peers }).await
}
