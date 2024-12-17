use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::Method,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use log::{error, info};
use serde::Deserialize;
use serde_json::json;
use std::{
    net::SocketAddr,
    sync::mpsc::channel,
    sync::{Arc, Mutex},
};
use tower_http::cors::{Any, CorsLayer};
use waku_bindings::{Running, WakuNodeHandle};

use de_mls::main_loop::{main_loop, Connection};
use ds::ds_waku::setup_node_handle;

struct AppState {
    node: Arc<WakuNodeHandle<Running>>,
    rooms: Mutex<Vec<String>>,
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let port = std::env::var("PORT")
        .map(|val| val.parse::<u16>())
        .unwrap_or(Ok(3000))
        .unwrap();
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let node_name = std::env::var("NODE").unwrap();

    let app_state = Arc::new(AppState {
        node: Arc::new(setup_node_handle(vec![node_name]).unwrap()),
        rooms: Mutex::new(Vec::new()),
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
    let res = axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await;
    if let Err(e) = res {
        error!("Error hosting server: {}", e);
    }
}

async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();
    let (tx_waku_to_ws, rx_waku_to_ws) = channel::<String>();
    let (tx_ws_to_waku, rx_ws_to_waku) = channel::<String>();
    let mut main_loop_connection = None::<Connection>;

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(data) = msg {
            #[derive(Deserialize, Debug)]
            struct Connect {
                eth_private_key: String,
                group_id: String,
                should_create: bool,
            }
            info!("Got data: {:?}", &data);
            let connect: Connect = match serde_json::from_str(&data) {
                Ok(connect) => {
                    info!("Got connect: {:?}", &connect);
                    connect
                }
                Err(err) => {
                    let msg = format!("Failed to get connection message: {:?}", err);
                    error!("{}", msg);
                    let _ = sender.send(Message::from(msg)).await;
                    break;
                }
            };

            main_loop_connection = Some(Connection {
                eth_private_key: connect.eth_private_key.clone(),
                group_id: connect.group_id.clone(),
                should_create_group: connect.should_create,
            });

            {
                let mut rooms = state.rooms.lock().unwrap();
                if !rooms.contains(&connect.group_id.clone()) {
                    rooms.push(connect.group_id.clone());
                } else {
                    error!("Group already exists");
                    continue;
                }
            }
            info!("Prepare info for main loop: {:?}", main_loop_connection);
            break;
        }
    }

    let group_id = main_loop_connection.as_ref().unwrap().group_id.clone();
    let mut main_loop_handle = tokio::spawn(async move {
        info!("Running main loop");
        let res = main_loop(
            main_loop_connection.unwrap().clone(),
            state.node.clone(),
            tx_waku_to_ws.clone(),
            rx_ws_to_waku,
        )
        .await;
        if let Err(e) = res {
            error!("Error running main loop: {}", e);
        } else {
            let mut rooms = state.rooms.lock().unwrap();
            rooms.push(group_id);
        }
    });

    let mut recv_messages = tokio::spawn(async move {
        info!("Running recv messages from waku");
        while let Ok(msg) = rx_waku_to_ws.recv() {
            info!("Got message from waku: {:?}", msg);
            if let Err(e) = sender.send(Message::Text(msg)).await {
                error!("Error sending message to client: {}", e);
                break;
            }
        }
    });

    let mut send_messages = {
        let tx = tx_ws_to_waku.clone();
        tokio::spawn(async move {
            info!("Running send messages to waku");
            while let Some(Ok(Message::Text(text))) = receiver.next().await {
                info!("Got message from ws: {:?}", text);
                let res = tx.send(text);
                if let Err(e) = res {
                    error!("Error sending message to waku: {}", e);
                }
            }
        })
    };

    info!("Waiting for main loop to finish");
    tokio::select! {
        _ = (&mut main_loop_handle) => {
            info!("main_loop_handle finished");
            recv_messages.abort();
            send_messages.abort()
        }
        _ = (&mut send_messages) => {info!("send_messages finished"); recv_messages.abort(); main_loop_handle.abort()},
        _ = (&mut recv_messages) => {info!("recv_messages finished"); send_messages.abort(); main_loop_handle.abort()},
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
