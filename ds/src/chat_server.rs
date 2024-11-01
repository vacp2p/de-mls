use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::{
    net::TcpListener,
    sync::{mpsc, Mutex},
};
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message};

use crate::DeliveryServiceError;

type Tx = mpsc::UnboundedSender<Message>;
type PeerMap = Arc<Mutex<HashMap<String, Tx>>>;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type")]
pub enum ServerMessage {
    InMessage {
        from: String,
        to: Vec<String>,
        msg: String,
    },
    SystemJoin {
        username: String,
    },
}

pub async fn start_server(addr: &str) -> Result<(), DeliveryServiceError> {
    let listener = TcpListener::bind(addr).await?;
    let peers = PeerMap::new(Mutex::new(HashMap::new()));

    while let Ok((stream, _)) = listener.accept().await {
        let peers = peers.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_connection(peers, stream).await {
                eprintln!("Error in connection handling: {:?}", e);
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    peers: PeerMap,
    stream: tokio::net::TcpStream,
) -> Result<(), DeliveryServiceError> {
    let ws_stream = accept_async(stream).await?;
    let (mut write, mut read) = ws_stream.split();
    let (sender, receiver) = mpsc::unbounded_channel();
    let receiver = Arc::new(Mutex::new(receiver));

    let mut username = String::new();

    // Spawn a task to handle outgoing messages
    tokio::spawn(async move {
        while let Some(message) = receiver.lock().await.recv().await {
            if let Err(err) = write.send(message).await {
                return Err(DeliveryServiceError::SenderError(err.to_string()));
            }
        }
        Ok(())
    });

    // Handle incoming messages
    while let Some(Ok(Message::Text(text))) = read.next().await {
        if let Ok(chat_message) = serde_json::from_str::<ServerMessage>(&text) {
            match chat_message {
                ServerMessage::SystemJoin {
                    username: join_username,
                } => {
                    username = join_username.clone();
                    peers
                        .lock()
                        .await
                        .insert(join_username.clone(), sender.clone());
                    println!("{} joined the chat", join_username);
                }
                ServerMessage::InMessage { from, to, msg } => {
                    println!("Received message from {} to {:?}: {}", from, to, msg);
                    for recipient in to {
                        if let Some(recipient_sender) = peers.lock().await.get(&recipient) {
                            let message = ServerMessage::InMessage {
                                from: from.clone(),
                                to: vec![recipient.clone()],
                                msg: msg.clone(),
                            };
                            let message_json = serde_json::to_string(&message).unwrap();
                            recipient_sender
                                .send(Message::Text(message_json))
                                .map_err(|err| {
                                    DeliveryServiceError::SenderError(err.to_string())
                                })?;
                        }
                    }
                }
            }
        }
    }

    // Remove the user from the map when they disconnect
    if !username.is_empty() {
        peers.lock().await.remove(&username);
        println!("{} left the chat", username);
    }
    Ok(())
}

#[tokio::test]
async fn start_test() {
    start_server("127.0.0.1:8080").await.unwrap()
}
