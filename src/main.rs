use alloy::{primitives::Address, providers::ProviderBuilder, signers::local::PrivateKeySigner};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::State,
    http::Method,
    response::IntoResponse,
    routing::get,
    Router,
};
use chrono::Utc;
use clap::Parser;
use futures::{SinkExt, StreamExt};
use sc_key_store::{sc_ks::ScKeyStorage, SCKeyStoreService};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    str::FromStr,
    sync::mpsc::{channel, Receiver, Sender},
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;
// use tokio::sync::{broadcast};
use log::{error, info};
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};
use waku_bindings::WakuMessage;

use de_mls::main_loop::{main_loop, Connection};
use de_mls::{
    cli::*,
    user::{AdminTrait, User},
    CliError,
};
use ds::ds_waku::setup_node_handle;

struct AppState {
    rooms: Mutex<Vec<String>>,
    // users: Mutex<HashMap<String, User>>,
}

struct RoomState {
    users: Mutex<HashSet<String>>,
    waku_sender: Sender<WakuMessage>,
}

impl RoomState {
    fn new(waku_sender: Option<Sender<WakuMessage>>) -> Self {
        Self {
            users: Mutex::new(HashSet::new()),
            waku_sender: waku_sender.unwrap(),
        }
    }
}

#[test]
fn require_fn_to_be_send() {
    fn require_send<T: Send>(_t: T) {}
    require_send(RoomState::new(None::<Sender<WakuMessage>>));
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let port = std::env::var("PORT")
        .map(|val| val.parse::<u16>())
        .unwrap_or(Ok(3000))
        .unwrap();
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let app_state = Arc::new(AppState {
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

    println!("Hosted on {}", addr.to_string());
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
    let (tx_ws, rx_ws) = channel::<String>();
    let (tx_waku, rx_waku) = channel::<String>();
    let mut main_loop_connection = None::<Connection>;

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(data) = msg {
            #[derive(Deserialize, Debug)]
            struct Connect {
                eth_private_key: String,
                group_id: String,
                node: String,
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
                node: connect.node.clone(),
                is_created: false,
            });

            {
                let rooms = state.rooms.lock().unwrap();
                if rooms.contains(&connect.group_id.clone()) {
                    main_loop_connection.as_mut().unwrap().is_created = true;
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
            tx_ws.clone(),
            rx_waku,
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
        while let Ok(msg) = rx_ws.recv() {
            if let Err(e) = sender.send(Message::Text(msg)).await {
                error!("Error sending message to client: {}", e);
                break;
            }
        }
    });

    let mut send_messages = {
        let tx = tx_waku.clone();
        tokio::spawn(async move {
            info!("Running send messages to waku");
            while let Some(Ok(Message::Text(text))) = receiver.next().await {
                let res = tx.send(text);
                if let Err(e) = res {
                    error!("Error sending message to waku: {}", e);
                }
            }
        })
    };

    tokio::select! {
        _ = (&mut send_messages) => {recv_messages.abort(); main_loop_handle.abort()},
        _ = (&mut recv_messages) => {send_messages.abort(); main_loop_handle.abort()},
        _ = (&mut main_loop_handle) => {
            recv_messages.abort();
            send_messages.abort()
        }
    };
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

// #[tokio::main]
// async fn main() -> Result<(), Box<dyn Error>> {
//     let token = CancellationToken::new();

//     let (cli_tx, mut cli_gr_rx) = mpsc::channel::<Commands>(100);

//     let args = Args::parse();
//     let signer = PrivateKeySigner::from_str(&args.user_priv_key)?;
//     let nodes_name: Vec<String> = vec![
//         "/ip4/143.110.189.47/tcp/60000/p2p/16Uiu2HAkufuEruSFrepFCtPS2sBuKwTCMPCrBMT188JHs4hTtFDY"
//             .to_string(),
//     ];
//     let user_address = signer.address().to_string();
//     let group_name: String = "new_group".to_string();
//     let node = setup_node_handle(nodes_name).unwrap();

//     // Create user
//     let user = User::new(&args.user_priv_key, node).await?;
//     let user_arc = Arc::new(Mutex::new(user));

//     let (messages_tx, messages_rx) = mpsc::channel::<Msg>(100);
//     messages_tx
//         .send(Msg::Input(Message::System(format!(
//             "Hello, {:}",
//             user_address.clone()
//         ))))
//         .await?;

//     let messages_tx2 = messages_tx.clone();
//     let event_token = token.clone();
//     let h1 = tokio::spawn(async move { event_handler(messages_tx2, cli_tx, event_token).await });

//     let res_msg_tx = messages_tx.clone();
//     let main_token = token.clone();
//     let user = user_arc.clone();

//     let h2 = tokio::spawn(async move {
//         let (waku_cg_to_cli_tx, mut waku_cg_to_cli_rx) = mpsc::channel::<WakuMessage>(100);
//         let (waku_sg_to_cli_tx, mut waku_sg_to_cli_rx) = mpsc::channel::<WakuMessage>(100);
//         loop {
//             tokio::select! {
//                 // Some(val) = waku_sg_to_cli_rx.recv() =>{
//                 //     println!("HERE SG TO CLI !!!");
//                 //     res_msg_tx.send(Msg::Input(Message::System("Get message from waku".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                 //     let res = user.as_ref().lock().await.process_waku_msg(val).await;
//                 //     match res {
//                 //         Ok(msg) => {
//                 //             match msg {
//                 //                 Some(m) => res_msg_tx.send(Msg::Input(Message::System(m))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
//                 //                 None => continue
//                 //             }
//                 //         },
//                 //         Err(err) => {
//                 //             res_msg_tx
//                 //                 .send(Msg::Input(Message::Error(err.to_string())))
//                 //                 .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                 //         },
//                 //     };
//                 // }
//                 // Some(val) = waku_cg_to_cli_rx.recv() =>{
//                 //     println!("HERE CG TO CLI !!!");
//                 //     res_msg_tx.send(Msg::Input(Message::System("Get message from waku".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                 //     let res = user.as_ref().lock().await.process_waku_msg(val).await;
//                 //     match res {
//                 //         Ok(msg) => {
//                 //             match msg {
//                 //                 Some(m) => res_msg_tx.send(Msg::Input(Message::System(m))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
//                 //                 None => continue
//                 //             }
//                 //         },
//                 //         Err(err) => {
//                 //             res_msg_tx
//                 //                 .send(Msg::Input(Message::Error(err.to_string())))
//                 //                 .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                 //         },
//                 //     };
//                 // }
//                 Some(command) = cli_gr_rx.recv() => {
//                     res_msg_tx.send(Msg::Input(Message::System(format!("Get command: {:?}", command)))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                     match command {
//                         Commands::CreateGroup {group_name} => {
//                             let res = user.as_ref().lock().await.create_group(group_name.clone());
//                             match res {
//                                 Ok(br) => {
//                                     let msg = format!("Successfully create group: {:?}", group_name.clone());
//                                     res_msg_tx.send(Msg::Input(Message::System(msg))).await.map_err(|err| CliError::SenderError(err.to_string()))?;

//                                     let res_msg_tx_clone = res_msg_tx.clone();
//                                     let user_clone = user_arc.clone();
//                                     tokio::spawn(async move {
//                                         while let Ok(msg) = br.recv() {
//                                             let res = user_clone.as_ref().lock().await.process_waku_msg(msg).await;
//                                             match res {
//                                                 Ok(msg) => {
//                                                     match msg {
//                                                         Some(m) => res_msg_tx_clone.send(Msg::Input(Message::System(m))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
//                                                         None => continue
//                                                     }
//                                                 },
//                                                 Err(err) => {
//                                                     res_msg_tx_clone
//                                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                                 },
//                                             }
//                                             // println!("SEND TO CLI");
//                                             // waku_cg_to_cli_tx_clone.send(msg).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                         }
//                                         Ok::<_, CliError>(())
//                                     });

//                                     // after create group run thread to send admin message each interval
//                                     let user_clone = user_arc.clone();
//                                     let group_name_clone = group_name.clone();
//                                     let res_msg_tx_clone = res_msg_tx.clone();
//                                     tokio::spawn(async move {
//                                         let mut interval = tokio::time::interval(Duration::from_secs(20));
//                                         loop {
//                                             interval.tick().await;
//                                             let res = send_admin_msg(&user_clone, group_name_clone.clone()).await.map_err(|err| CliError::SenderError(err.to_string()));
//                                             match res {
//                                                 Ok(msg) => {
//                                                     res_msg_tx_clone.send(Msg::Input(Message::System(format!("Successfully publish message with id: {}", msg)))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                                 },
//                                                 Err(err) => {
//                                                     res_msg_tx_clone.send(Msg::Input(Message::Error(err.to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                                 },
//                                             }
//                                         }
//                                         Ok::<_, CliError>(())
//                                     });
//                                 },
//                                 Err(err) => {
//                                     res_msg_tx
//                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                             };
//                         },
//                         Commands::SendMessage { group_name, msg } => {
//                             let message = msg.join(" ");
//                             let res = user.as_ref().lock().await.send_msg(&message, group_name.clone()).await;
//                             match res {
//                                 Ok(_) => {
//                                     res_msg_tx.send(Msg::Input(Message::Mine(group_name, user_address.clone(), message ))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                                 Err(err) => {
//                                     res_msg_tx
//                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                             };
//                         },
//                         Commands::Subscribe { group_name } => {
//                             let res = user.as_ref().lock().await.subscribe_to_group(group_name.clone()).await;
//                             match res {
//                                 Ok(receiver) => {
//                                     let msg = format!("Successfully subscribe to group: {:?}", group_name.clone());
//                                     res_msg_tx.send(Msg::Input(Message::System(msg))).await.map_err(|err| CliError::SenderError(err.to_string()))?;

//                                     // let waku_sg_to_cli_tx_clone = waku_sg_to_cli_tx.clone();
//                                     let user_clone = user_arc.clone();
//                                     let res_msg_tx_clone = res_msg_tx.clone();
//                                     tokio::spawn(async move {
//                                         while let Ok(msg) = receiver.recv() {
//                                             let res = user_clone.as_ref().lock().await.process_waku_msg(msg).await;
//                                             match res {
//                                                 Ok(msg) => {
//                                                     match msg {
//                                                         Some(m) => res_msg_tx_clone.send(Msg::Input(Message::System(m))).await.map_err(|err| CliError::SenderError(err.to_string()))?,
//                                                         None => continue
//                                                     }
//                                                 },
//                                                 Err(err) => {
//                                                     res_msg_tx_clone
//                                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                                 },
//                                             }
//                                         }
//                                         Ok::<_, CliError>(())
//                                     });
//                                 },
//                                 Err(err) => {
//                                     res_msg_tx
//                                         .send(Msg::Input(Message::Error(err.to_string())))
//                                         .await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                                 },
//                             };
//                         },
//                         Commands::Exit => {
//                             res_msg_tx.send(Msg::Input(Message::System("Bye!".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                             break
//                         },
//                     }
//                 }
//                 _ = main_token.cancelled() => {
//                     break;
//                 }
//                 else => {
//                     res_msg_tx.send(Msg::Input(Message::System("Something went wrong".to_string()))).await.map_err(|err| CliError::SenderError(err.to_string()))?;
//                     break
//                 }
//             };
//         }
//         Ok::<_, CliError>(())
//     });

//     let h3 = tokio::spawn(async move { terminal_handler(messages_rx, token).await });

//     let handler_res = tokio::join!(h1, h2, h3);
//     handler_res.0??;
//     handler_res.1??;
//     handler_res.2??;

//     Ok(())
// }

// async fn send_admin_msg(
//     user: &Arc<Mutex<User>>,
//     group_name: String,
// ) -> Result<String, Box<dyn Error>> {
//     let mut user = user.as_ref().lock().await;
//     let group = user.groups.get_mut(&group_name.clone()).unwrap();
//     group.admin.as_mut().unwrap().generate_new_key_pair()?;
//     let admin_msg = group.admin.as_mut().unwrap().generate_admin_message();
//     let res = user.send_admin_msg(admin_msg, group_name.clone()).await;
//     match res {
//         Ok(msg_id) => return Ok(msg_id),
//         Err(e) => return Err(e.to_string().into()),
//     }
// }
