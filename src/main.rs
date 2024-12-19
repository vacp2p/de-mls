use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::Method,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::StreamExt;
use kameo::actor::ActorRef;
use log::{error, info};
use serde_json::json;
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::{channel, Sender};
use tower_http::cors::{Any, CorsLayer};
use waku_bindings::{waku_set_event_callback, WakuMessage};

use de_mls::{
    main_loop::{main_loop, Connection},
    user::{ProcessSendMessage, User, UserAction},
    ws_actor::{RawWsMessage, WsAction, WsActor},
    AppState, MessageToPrint,
};
use ds::{
    ds_waku::{match_content_topic, setup_node_handle},
    waku_actor::WakuActor,
};

#[tokio::main]
async fn main() {
    env_logger::init();
    let port = std::env::var("PORT")
        .map(|val| val.parse::<u16>())
        .unwrap_or(Ok(3000))
        .unwrap();
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let node_name = std::env::var("NODE").unwrap();
    let node = setup_node_handle(vec![node_name]).unwrap();
    let uuid = uuid::Uuid::new_v4().as_bytes().to_vec();
    let waku_actor = kameo::actor::spawn(WakuActor::new(Arc::new(node), uuid.clone()));
    let (tx, _) = tokio::sync::broadcast::channel(100);
    let app_state = Arc::new(AppState {
        waku_actor,
        rooms: Mutex::new(HashSet::new()),
        app_id: uuid.clone(),
        content_topics: Arc::new(Mutex::new(Vec::new())),
        pubsub: tx.clone(),
    });

    let (waku_sender, mut waku_receiver) = channel::<WakuMessage>(100);
    handle_waku(waku_sender, app_state.clone()).await;

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
}

async fn handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_waku(waku_sender: Sender<WakuMessage>, state: Arc<AppState>) {
    info!("Setting up waku event callback");
    let mut seen_messages = Vec::<String>::new();
    waku_set_event_callback(move |signal| {
        match signal.event() {
            waku_bindings::Event::WakuMessage(event) => {
                let msg_id = event.message_id();
                if seen_messages.contains(msg_id) {
                    return;
                }
                seen_messages.push(msg_id.clone());
                let msg_app_id = event.waku_message().meta();
                if msg_app_id == state.app_id {
                    return;
                };
                let content_topic = event.waku_message().content_topic();
                // Check if message belongs to a relevant topic
                if !match_content_topic(&state.content_topics, content_topic) {
                    error!("Content topic not match: {:?}", content_topic);
                };
                let msg = event.waku_message().clone();
                info!("Received message from waku: {:?}", event.message_id());
                waku_sender.blocking_send(msg).unwrap();
            }

            waku_bindings::Event::Unrecognized(data) => {
                error!("Unrecognized event!\n {data:?}");
            }
            _ => {
                error!(
                    "Unrecognized signal!\n {:?}",
                    serde_json::to_string(&signal)
                );
            }
        }
    });
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (ws_sender, mut ws_receiver) = socket.split();
    let ws_actor = kameo::spawn(WsActor::new(ws_sender));
    let mut main_loop_connection = None::<Connection>;

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
                    info!("Prepare info for main loop: {:?}", main_loop_connection);
                    break;
                } else {
                    info!("Group already exists");
                    continue;
                }
            }
            Ok(WsAction::UserMessage(msg)) => {
                info!("Got chat message for non-existent user: {:?}", &msg)
            }

            Err(e) => error!("Error handling message: {}", e),
        }
    }

    let user_actor = main_loop(main_loop_connection.unwrap().clone(), state.clone())
        .await
        .expect("Failed to start main loop");

    let user_actor_clone = user_actor.clone();
    let state_clone = state.clone();
    let ws_actor_clone = ws_actor.clone();
    let mut waku_receiver = state.pubsub.subscribe();
    let mut recv_messages = tokio::spawn(async move {
        info!("Running recv messages from waku");
        while let Ok(msg) = waku_receiver.recv().await {
            let res = handle_user_actions(
                msg,
                state_clone.waku_actor.clone(),
                ws_actor_clone.clone(),
                user_actor_clone.clone(),
            )
            .await;
            if let Err(e) = res {
                error!("Error handling waku message: {}", e);
            }
        }
    });

    let user_ref_clone = user_actor.clone();
    let mut send_messages = {
        tokio::spawn(async move {
            info!("Running recieve messages from websocket");
            while let Some(Ok(Message::Text(text))) = ws_receiver.next().await {
                let res = handle_ws_message(
                    RawWsMessage { message: text },
                    ws_actor.clone(),
                    user_ref_clone.clone(),
                    state.waku_actor.clone(),
                )
                .await;
                if let Err(e) = res {
                    error!("Error handling websocket message: {}", e);
                }
            }
        })
    };

    info!("Waiting for main loop to finish");
    tokio::select! {
        _ = (&mut recv_messages) => {
            info!("recv_messages finished");
            send_messages.abort();
        }
        _ = (&mut send_messages) => {
            info!("send_messages finished");
            recv_messages.abort();
        }
    };
    info!("Main loop finished");
}

async fn handle_user_actions(
    msg: WakuMessage,
    waku_actor: ActorRef<WakuActor>,
    ws_actor: ActorRef<WsActor>,
    user_actor: ActorRef<User>,
) -> Result<(), Box<dyn std::error::Error>> {
    let actions = user_actor.ask(msg).await?;
    for action in actions {
        match action {
            UserAction::SendToWaku(msg) => {
                let id = waku_actor.ask(msg).await?;
                info!("Successfully publish message with id: {:?}", id);
            }
            UserAction::SendToGroup(msg) => {
                ws_actor.ask(msg).await?;
            }
            _ => {}
        }
    }
    Ok(())
}

async fn handle_ws_message(
    msg: RawWsMessage,
    ws_actor: ActorRef<WsActor>,
    user_actor: ActorRef<User>,
    waku_actor: ActorRef<WakuActor>,
) -> Result<(), Box<dyn std::error::Error>> {
    let action = ws_actor.ask(msg).await?;
    match action {
        WsAction::Connect(connect) => {
            info!("Got unexpected connect: {:?}", &connect);
        }
        WsAction::UserMessage(msg) => {
            info!("Got user message: {:?}", &msg);
            let mtp = MessageToPrint {
                message: msg.message.clone(),
                group_name: msg.group_id.clone(),
                sender: "me".to_string(),
            };
            ws_actor.ask(mtp).await?;

            let pmt = user_actor
                .ask(ProcessSendMessage {
                    msg: msg.message,
                    group_name: msg.group_id,
                })
                .await?;
            let id = waku_actor.ask(pmt).await?;
            info!("Successfully publish message with id: {:?}", id);
        }
    }

    Ok(())
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
