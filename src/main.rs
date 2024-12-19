use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::Method,
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{SinkExt, StreamExt};
use kameo::{actor::pubsub::PubSub, actor::ActorRef};
use log::{error, info};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::{channel, Sender};
use tower_http::cors::{Any, CorsLayer};
use waku_bindings::{
    waku_set_event_callback, Running, WakuContentTopic, WakuMessage, WakuNodeHandle,
};

use de_mls::{
    main_loop::{main_loop, Connection},
    user::{ProcessSendMessage, UserAction},
    ws_actor::ConnectMessage,
};
use ds::{
    ds_waku::{
        build_content_topics, content_filter, match_content_topic, pubsub_topic, setup_node_handle,
        GROUP_VERSION, SUBTOPICS,
    },
    waku_actor::{ProcessMessageToSend, WakuActor},
};

struct AppState {
    node: ActorRef<WakuActor>,
    rooms: Mutex<HashSet<String>>,
    app_id: Vec<u8>,
    content_topics: Arc<Mutex<Vec<WakuContentTopic>>>,
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
    let node = setup_node_handle(vec![node_name]).unwrap();
    let uuid = uuid::Uuid::new_v4().as_bytes().to_vec();
    let waku_actor = kameo::actor::spawn(WakuActor::new(Arc::new(node), uuid.clone()));
    let app_state = Arc::new(AppState {
        node: waku_actor,
        rooms: Mutex::new(HashSet::new()),
        app_id: uuid.clone(),
        content_topics: Arc::new(Mutex::new(Vec::new())),
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

async fn handle_waku(waku_sender: Sender<WakuMessage>, state: Arc<AppState>) {
    info!("Setting up waku event callback");
    let mut seen_messages = Vec::<String>::new();
    waku_set_event_callback(move |signal| {
        match signal.event() {
            waku_bindings::Event::WakuMessage(event) => {
                let msg_id = event.message_id();
                if seen_messages.contains(&msg_id) {
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
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut main_loop_connection = None::<Connection>;

    while let Some(Ok(msg)) = ws_receiver.next().await {
        if let Message::Text(data) = msg {
            info!("Got data: {:?}", &data);
            let connect: ConnectMessage = match serde_json::from_str(&data) {
                Ok(connect) => {
                    info!("Got connect: {:?}", &connect);
                    connect
                }
                Err(err) => {
                    let msg = format!("Failed to get connection message: {:?}", err);
                    error!("{}", msg);
                    let _ = ws_sender.send(Message::from(msg)).await;
                    break;
                }
            };

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
    }

    let group_id = main_loop_connection.as_ref().unwrap().group_id.clone();

    let mut content_topics =
        build_content_topics(&group_id.clone(), GROUP_VERSION, &SUBTOPICS.clone());
    state
        .content_topics
        .lock()
        .unwrap()
        .append(&mut content_topics);

    let (waku_sender, mut waku_receiver) = channel::<WakuMessage>(100);
    handle_waku(waku_sender, state.clone()).await;
    let user_ref = main_loop(main_loop_connection.unwrap().clone(), state.node.clone())
        .await
        .expect("Failed to start main loop");

    let user_ref_clone = user_ref.clone();
    let state_clone = state.clone();
    let mut recv_messages = tokio::spawn(async move {
        info!("Running recv messages from waku");
        while let Some(msg) = waku_receiver.recv().await {
            let res = user_ref_clone.ask(msg).await;
            match res {
                Ok(actions) => {
                    for action in actions {
                        match action {
                            UserAction::SendToWaku(msg) => {
                                let res = state_clone.node.ask(msg).await;
                                match res {
                                    Ok(id) => {
                                        info!("Successfully publish message with id: {:?}", id);
                                    }
                                    Err(e) => {
                                        error!("Error sending message to waku: {}", e);
                                    }
                                }
                            }
                            UserAction::SendToGroup(msg) => {
                                let res = ws_sender.send(Message::Text(msg)).await;
                                if let Err(e) = res {
                                    error!("Error sending message to ws: {}", e);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Err(e) => {
                    error!("Error handling message: {}", e);
                }
            }
        }
    });

    let user_ref_clone = user_ref.clone();
    let mut send_messages = {
        tokio::spawn(async move {
            info!("Running recieve messages from websocket");
            while let Some(Ok(Message::Text(text))) = ws_receiver.next().await {
                info!("Got message from ws: {:?}", text);
                #[derive(Serialize, Deserialize, Debug)]
                struct WsMessage {
                    message: String,
                    group_id: String,
                }
                let ws_message: WsMessage =
                    serde_json::from_str(&text).expect("Failed to parse message");
                if ws_message.group_id != group_id {
                    continue;
                }

                let res = user_ref_clone
                    .ask(ProcessSendMessage {
                        msg: ws_message.message,
                        group_name: ws_message.group_id,
                    })
                    .await;
                match res {
                    Ok(pmt) => {
                        let res = state.node.ask(pmt).await;
                        match res {
                            Ok(id) => {
                                info!("Successfully publish message with id: {:?}", id);
                            }
                            Err(e) => {
                                error!("Error sending message to waku: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("Error sending message to waku: {}", e);
                    }
                }
            }
        })
    };

    info!("Waiting for main loop to finish");
    tokio::select! {
        x = (&mut recv_messages) => {
            // info!("recv_messages finished");
        }
        w = (&mut send_messages) => {
            // info!("send_messages finished");
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
