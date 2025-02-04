use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::Method,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::StreamExt;
use log::{error, info};
use serde_json::json;
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::channel;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};
use waku_bindings::WakuMessage;

use de_mls::{
    action_handlers::{handle_user_actions, handle_ws_action},
    user_app_instance::create_user_instance,
    ws_actor::{RawWsMessage, WsAction, WsActor},
    AppState, Connection,
};
use ds::waku_actor::{ProcessMessageToSend, WakuNode};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let port = std::env::var("PORT")
        .map(|val| val.parse::<u16>())
        .unwrap_or(Ok(3000))?;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let node_port = std::env::var("NODE_PORT").expect("NODE_PORT is not set");

    let content_topics = Arc::new(Mutex::new(Vec::new()));

    let (waku_sender, mut waku_receiver) = channel::<WakuMessage>(100);
    let waku_node_init = WakuNode::new(node_port.parse::<usize>().unwrap()).await?;
    let waku_node = waku_node_init
        .start(waku_sender, content_topics.clone())
        .await?;

    let peer_addresses =
        ["/ip4/139.59.24.82/tcp/60000/p2p/16Uiu2HAm34X7nzHh7t4mbDDEt62GFPky8tAvqLUoqrZFSXr2bG7d"];
    waku_node
        .connect_to_peers(peer_addresses.iter().map(|s| s.parse().unwrap()).collect())
        .await?;

    let (sender, mut reciever) = channel::<ProcessMessageToSend>(100);
    tokio::task::block_in_place(move || {
        tokio::runtime::Handle::current().block_on(async move {
            while let Some(msg) = reciever.recv().await {
                let id = waku_node
                    .send_message(msg)
                    .await
                    .expect("Failed to send message to waku");
                info!("Successfully publish message with id: {:?}", id);
            }
        });
    });

    let (tx, _) = tokio::sync::broadcast::channel(100);
    let app_state = Arc::new(AppState {
        waku_node: sender,
        rooms: Mutex::new(HashSet::new()),
        content_topics,
        pubsub: tx.clone(),
    });

    let recv_messages = tokio::spawn(async move {
        info!("Running recv messages from waku");
        while let Some(msg) = waku_receiver.recv().await {
            let _ = tx.send(msg);
        }
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(vec![Method::GET]);

    let app = Router::new()
        .route("/", get(|| async { "Hello World!" }))
        .route("/ws", get(handler))
        .route("/rooms", get(get_rooms))
        .with_state(app_state)
        .layer(cors);

    println!("Hosted on {:?}", addr);

    let res = axum::Server::bind(&addr).serve(app.into_make_service());
    tokio::select! {
        Err(x) = res => {
            error!("Error hosting server: {}", x);
        }
        Err(w) = recv_messages => {
            error!("Error receiving messages from waku: {}", w);
        }
    }
    Ok(())
}

async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (ws_sender, mut ws_receiver) = socket.split();
    let ws_actor = kameo::spawn(WsActor::new(ws_sender));
    let mut main_loop_connection = None::<Connection>;
    let cancel_token = CancellationToken::new();
    while let Some(Ok(Message::Text(data))) = ws_receiver.next().await {
        let res = ws_actor.ask(RawWsMessage { message: data }).await;
        match res {
            Ok(WsAction::Connect(connect)) => {
                info!("Got connect: {:?}", &connect);
                main_loop_connection = Some(Connection {
                    eth_private_key: connect.eth_private_key.clone(),
                    group_id: connect.group_id.clone(),
                    should_create_group: connect.should_create,
                });
                let mut rooms = state.rooms.lock().unwrap();
                if !rooms.contains(&connect.group_id.clone()) {
                    rooms.insert(connect.group_id.clone());
                }
                info!("Prepare info for main loop: {:?}", main_loop_connection);
                break;
            }
            Ok(_) => {
                info!("Got chat message for non-existent user");
            }

            Err(e) => error!("Error handling message: {}", e),
        }
    }

    let user_actor = create_user_instance(main_loop_connection.unwrap().clone(), state.clone())
        .await
        .expect("Failed to start main loop");

    let user_actor_clone = user_actor.clone();
    let state_clone = state.clone();
    let ws_actor_clone = ws_actor.clone();
    let cancel_token_clone = cancel_token.clone();

    let mut user_waku_receiver = state.pubsub.subscribe();
    let mut recv_messages_waku = tokio::spawn(async move {
        info!("Running recv messages from waku for current user");
        while let Ok(msg) = user_waku_receiver.recv().await {
            let res = handle_user_actions(
                msg,
                state_clone.waku_node.clone(),
                ws_actor_clone.clone(),
                user_actor_clone.clone(),
                cancel_token_clone.clone(),
            )
            .await;
            if let Err(e) = res {
                error!("Error handling waku message: {}", e);
            }
        }
    });

    let user_ref_clone = user_actor.clone();
    let mut recv_messages_ws = {
        tokio::spawn(async move {
            info!("Running recieve messages from websocket");
            while let Some(Ok(Message::Text(text))) = ws_receiver.next().await {
                let res = handle_ws_action(
                    RawWsMessage { message: text },
                    ws_actor.clone(),
                    user_ref_clone.clone(),
                    state.waku_node.clone(),
                )
                .await;
                if let Err(e) = res {
                    error!("Error handling websocket message: {}", e);
                }
            }
        })
    };

    tokio::select! {
        _ = (&mut recv_messages_waku) => {
            info!("recv messages from waku finished");
            recv_messages_ws.abort();
        }
        _ = (&mut recv_messages_ws) => {
            info!("recieve messages from websocket finished");
            recv_messages_ws.abort();
        }
        _ = cancel_token.cancelled() => {
            info!("Cancel token cancelled");
            recv_messages_ws.abort();
            recv_messages_waku.abort();
        }
    };

    info!("Main loop finished");
}

async fn get_rooms(State(state): State<Arc<AppState>>) -> String {
    let rooms = state.rooms.lock().unwrap();
    let vec = rooms.iter().collect::<Vec<&String>>();
    match vec.len() {
        0 => json!({
            "status": "No rooms found yet!",
            "rooms": []
        })
        .to_string(),
        _ => json!({
            "status": "Success!",
            "rooms": vec
        })
        .to_string(),
    }
}
