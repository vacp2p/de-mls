use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::Method,
    response::IntoResponse,
    routing::get,
    Router,
};
use axum_server::Server;
use futures::StreamExt;
use log::{error, info};
use std::{net::SocketAddr, sync::Arc};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};
use waku_bindings::Multiaddr;

use de_mls::{
    app_runtime::{start_node, SessionHandle},
    topic_filter::TopicFilter,
};
use de_mls::{
    ws_actor::{RawWsMessage, WsAction, WsActor},
    AppState, Connection,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let port = std::env::var("PORT")
        .map(|val| val.parse::<u16>())
        .unwrap_or(Ok(3000))?;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let node_port = std::env::var("NODE_PORT").expect("NODE_PORT is not set");
    let peer_addresses: Vec<Multiaddr> = std::env::var("PEER_ADDRESSES")
        .expect("PEER_ADDRESSES is not set")
        .split(',')
        .map(|addr| {
            addr.parse::<Multiaddr>()
                .expect("Failed to parse peer address")
        })
        .collect();

    let node = start_node(node_port, peer_addresses).await?;
    info!("Node runtime initialized");

    // 2) Start HTTP server
    let app_state = node.app_state.clone();
    let server_task = tokio::spawn(async move {
        info!("Running server");
        run_server(app_state, addr)
            .await
            .expect("Failed to run server");
    });

    // 3) Observe node tasks
    tokio::select! {
        result = server_task => {
            if let Err(e) = result {
                error!("Error hosting server: {e}");
            }
        }
        result = node.fanout_task => {
            if let Err(e) = result {
                error!("Fanout task crashed: {e}");
            }
        }
        result = node.waku_join => {
            match result {
                Ok(Ok(_)) => info!("Waku node exited normally"),
                Ok(Err(e)) => error!("Waku node error: {e:#}"),
                Err(join_err) => error!("Join error waiting waku thread: {join_err:#}"),
            }
        }
    }

    Ok(())
}

async fn run_server(
    app_state: Arc<AppState>,
    addr: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let cors = CorsLayer::new()
        .allow_methods([Method::GET])
        .allow_origin(Any);

    let app = Router::new()
        .route("/", get(|| async { "Hello World!" }))
        .route("/ws", get(handler))
        // .route("/rooms", get(get_rooms))
        .with_state(app_state)
        .layer(cors);

    info!("Hosted on {addr:?}");

    Server::bind(addr).serve(app.into_make_service()).await?;
    Ok(())
}

async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let t = TopicFilter::new();
    let topics = Arc::new(t);
    info!("Handling socket");
    // Split WS and create WsActor just like before
    let (ws_sender, mut ws_receiver) = socket.split();
    let ws_actor = kameo::spawn(WsActor::new(ws_sender));

    // === Initial “Connect” handshake (unchanged) ===
    let mut main_loop_connection = None::<Connection>;
    let cancel_token = CancellationToken::new();
    while let Some(Ok(Message::Text(data))) = ws_receiver.next().await {
        let res = ws_actor
            .ask(RawWsMessage {
                message: data.to_string(),
            })
            .await;
        match res {
            Ok(WsAction::Connect(connect)) => {
                info!("Got connect: {:?}", &connect);
                main_loop_connection = Some(Connection {
                    eth_private_key: connect.eth_private_key.clone(),
                    group_id: connect.group_id.clone(),
                    should_create_group: connect.should_create,
                });
                // // Track room
                // if let Ok(mut rooms) = state.rooms.lock() {
                //     rooms.insert(connect.group_id.clone());
                // }
                break;
            }
            Ok(_) => {
                info!("Got chat message for non-existent user");
            }
            Err(e) => error!("Error handling message: {e}"),
        }
    }

    let connection = match main_loop_connection {
        Some(c) => c,
        None => {
            error!("No Connect message received; closing session");
            return;
        }
    };

    // === Start per-user session with extracted runtime ===
    // This internally spawns:
    //   - Waku → handle_user_actions
    //   - WS   → handle_ws_action
    let SessionHandle {
        mut recv_waku,
        mut recv_ws,
        ..
    } = de_mls::app_runtime::spawn_user_session(
        state.clone(),
        topics,
        connection,
        ws_actor.clone(),
        ws_receiver,
    )
    .await
    .expect("Failed to spawn user session");

    // Wait for either loop to end or cancellation
    tokio::select! {
        _ = (&mut recv_waku) => {
            info!("recv messages from waku finished");
            recv_ws.abort();
        }
        _ = (&mut recv_ws) => {
            info!("receive messages from websocket finished");
            recv_waku.abort();
        }
        _ = cancel_token.cancelled() => {
            info!("Cancel token cancelled");
            recv_ws.abort();
            recv_waku.abort();
        }
    };

    info!("Session finished");
}

// async fn get_rooms(State(state): State<Arc<AppState>>) -> String {
//     let rooms = match state.rooms.lock() {
//         Ok(rooms) => rooms,
//         Err(e) => {
//             log::error!("Failed to acquire rooms lock: {e}");
//             return json!({
//                 "status": "Error acquiring rooms lock",
//                 "rooms": []
//             })
//             .to_string();
//         }
//     };
//     let vec = rooms.iter().collect::<Vec<&String>>();
//     match vec.len() {
//         0 => json!({ "status": "No rooms found yet!", "rooms": [] }).to_string(),
//         _ => json!({ "status": "Success!", "rooms": vec }).to_string(),
//     }
// }
