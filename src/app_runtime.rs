use anyhow::{anyhow, Result};
use axum::extract::ws::WebSocket;
use futures::stream::SplitStream;
use futures::StreamExt;
use kameo::actor::ActorRef;
use log::{error, info};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    thread,
};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use waku_bindings::{Multiaddr, WakuContentTopic, WakuMessage};

use ds::waku_actor::{run_waku_node, WakuMessageToSend};

use crate::{
    action_handlers::{handle_user_actions, handle_ws_action},
    match_content_topic,
    topic_filter::TopicFilter,
    user_app_instance::create_user_instance_with_group,
    ws_actor::{RawWsMessage, WsAction, WsActor},
    AppState, Connection,
};

/// Handle to the running node and fanout tasks.
pub struct NodeHandle {
    pub app_state: Arc<AppState>,
    /// Task that relays Waku messages -> broadcast channel (`app_state.pubsub`).
    pub fanout_task: JoinHandle<()>,
    /// Join handle that waits for the dedicated Waku thread to exit.
    pub waku_join: JoinHandle<anyhow::Result<()>>,
}

/// Spawns the Waku node on a dedicated OS thread with its own current-thread runtime
/// and returns a `NodeHandle` you can await/monitor from your async context.
///
/// This preserves the “blocking forever” semantics of `run_waku_node` without
/// blocking your Tokio worker threads.
pub async fn start_node(node_port: String, peers: Vec<Multiaddr>) -> Result<NodeHandle> {
    // Shared channels & state — same shapes as your previous main.rs
    let content_topics = Arc::new(tokio::sync::RwLock::new(Vec::<WakuContentTopic>::new()));
    let (waku_sender, mut waku_receiver) = mpsc::channel::<WakuMessage>(100);
    let (outbound_tx, mut outbound_rx) = mpsc::channel::<WakuMessageToSend>(100);
    let (pubsub_tx, _) = broadcast::channel::<WakuMessage>(100);

    let app_state = Arc::new(AppState {
        waku_node: outbound_tx.clone(),
        pubsub: pubsub_tx.clone(),
    });

    // Fan-out Waku → broadcast
    let fanout_task = tokio::spawn({
        let pubsub_tx = pubsub_tx.clone();
        async move {
            info!("Running recv messages from waku (fanout)");
            while let Some(msg) = waku_receiver.recv().await {
                let _ = pubsub_tx.send(msg);
            }
        }
    });

    // Dedicated OS thread for Waku node (keeps your run_waku_node signature intact)
    let waku_thread = start_waku_blocking_thread(
        node_port.clone(),
        peers.clone(),
        waku_sender.clone(),
        outbound_rx, // moved; passed by &mut inside the thread
    );

    // Wrap thread join with a Tokio blocking task so you can select! on it
    let waku_join = tokio::task::spawn_blocking(move || match waku_thread.join() {
        Ok(res) => res,
        Err(panic) => Err(anyhow!("Waku thread panicked: {:?}", panic)),
    });

    Ok(NodeHandle {
        app_state,
        fanout_task,
        waku_join,
    })
}

/// Start a per-user session: subscribe to Waku, create user actor, and run:
///   - Waku → `handle_user_actions`
///   - WS   → `handle_ws_action`
///
/// You call this *after* you parse the initial `Connect` message and build `Connection`.
pub async fn spawn_user_session(
    app_state: Arc<AppState>,
    topics: Arc<TopicFilter>,
    connection: Connection,
    ws_actor: ActorRef<WsActor>,
    mut ws_receiver: SplitStream<WebSocket>,
) -> Result<SessionHandle> {
    // Create per-user actor instance
    let user_actor = create_user_instance_with_group(
        connection.clone(),
        app_state.clone(),
        ws_actor.clone(),
        topics.clone(),
    )
    .await
    .map_err(|e| anyhow!("create_user_instance: {e}"))?;

    let cancel = CancellationToken::new();

    // Waku → user
    let mut user_waku_rx = app_state.pubsub.subscribe();
    let app_state_waku = app_state.clone();
    let ws_actor_waku = ws_actor.clone();
    let user_actor_waku = user_actor.clone();
    let cancel_waku = cancel.clone();

    let recv_waku = tokio::spawn(async move {
        info!("Running recv messages from waku for current user");
        while let Ok(msg) = user_waku_rx.recv().await {
            let content_topic = msg.content_topic.clone();
            if !topics.contains(&content_topic).await {
                continue;
            }
            if let Err(e) = handle_user_actions(
                msg,
                app_state_waku.waku_node.clone(),
                ws_actor_waku.clone(),
                user_actor_waku.clone(),
                topics.clone(),
                cancel_waku.clone(),
            )
            .await
            {
                error!("Error handling waku message: {e}");
            }
        }
    });

    // WS → user  (reuse your existing `handle_ws_action`)
    let app_state_ws = app_state.clone();
    let ws_actor_ws = ws_actor.clone();
    let user_actor_ws = user_actor.clone();
    let recv_ws = tokio::spawn(async move {
        info!("Running receive messages from websocket");
        while let Some(Ok(axum::extract::ws::Message::Text(text))) = ws_receiver.next().await {
            if let Err(e) = handle_ws_action(
                RawWsMessage {
                    message: text.to_string(),
                },
                ws_actor_ws.clone(),
                user_actor_ws.clone(),
                app_state_ws.waku_node.clone(),
            )
            .await
            {
                error!("Error handling websocket message: {e}");
            }
        }
    });

    Ok(SessionHandle {
        cancel,
        recv_waku,
        recv_ws,
    })
}

/// Returned by `spawn_user_session` to manage the running tasks (optional to store).
pub struct SessionHandle {
    pub cancel: CancellationToken,
    pub recv_waku: JoinHandle<()>,
    pub recv_ws: JoinHandle<()>,
}

// -------------------- internal: dedicated Waku thread --------------------

fn start_waku_blocking_thread(
    node_port: String,
    peers: Vec<Multiaddr>,
    waku_sender: mpsc::Sender<WakuMessage>,
    mut outbound_rx: mpsc::Receiver<WakuMessageToSend>,
) -> thread::JoinHandle<anyhow::Result<()>> {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build waku rt");
        let result = rt.block_on(async move {
            run_waku_node(node_port, Some(peers), waku_sender, &mut outbound_rx).await
        });
        // Map any error to anyhow::Error to match the return type
        result.map_err(anyhow::Error::from)
    })
}
