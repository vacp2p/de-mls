use alloy::primitives::Address;
use alloy::signers::Signature;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::chat_server::ServerMessage;
use crate::ChatServiceError;

// pub const REQUEST: &str = "You are joining the group with smart contract: ";

#[derive(Serialize, Deserialize, Debug)]
pub enum ChatMessages {
    Request(RequestMLSPayload),
    Response(ResponseMLSPayload),
    Welcome(String),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ReqMessageType {
    InviteToGroup,
    RemoveFromGroup,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RequestMLSPayload {
    pub msg: String,
    pub msg_type: ReqMessageType,
}

impl RequestMLSPayload {
    pub fn new(sc_address: String, msg_type: ReqMessageType) -> Self {
        RequestMLSPayload {
            msg: sc_address,
            msg_type,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ResponseMLSPayload {
    signature: String,
    user_address: String,
    key_package: Vec<u8>,
}

impl ResponseMLSPayload {
    pub fn new(signature: String, user_address: String, key_package: Vec<u8>) -> Self {
        Self {
            signature,
            user_address,
            key_package,
        }
    }

    pub fn validate(&self, sc_address: String) -> Result<(String, Vec<u8>), ChatServiceError> {
        let recover_sig: Signature = serde_json::from_str(&self.signature)?;
        let addr = Address::from_str(&self.user_address)?;
        // Recover the signer from the message.
        let recovered = recover_sig.recover_address_from_msg(sc_address)?;

        if recovered.ne(&addr) {
            return Err(ChatServiceError::ValidationError);
        }
        Ok((self.user_address.clone(), self.key_package.clone()))
    }
}

pub struct ChatClient {
    sender: mpsc::UnboundedSender<Message>,
}

impl ChatClient {
    pub async fn connect(
        addr: &str,
        username: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Message>), ChatServiceError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(addr).await?;
        let (mut write, read) = ws_stream.split();
        let (sender, receiver) = mpsc::unbounded_channel();
        let (msg_sender, msg_receiver) = mpsc::unbounded_channel();

        let receiver = Arc::new(Mutex::new(receiver));

        // Spawn a task to handle outgoing messages
        tokio::spawn(async move {
            while let Some(message) = receiver.lock().await.recv().await {
                // println!("Message from reciever: {}", message);
                if let Err(e) = write.send(message).await {
                    eprintln!("Error sending message: {}", e);
                }
            }
        });

        // Spawn a task to handle incoming messages
        tokio::spawn(async move {
            let mut read = read;
            while let Some(message) = read.next().await {
                if let Ok(msg) = message {
                    if let Err(e) = msg_sender.send(msg) {
                        eprintln!("Failed to send message to channel: {}", e);
                    }
                }
            }
        });

        // Send a SystemJoin message when registering
        let join_msg = ServerMessage::SystemJoin {
            username: username.to_string(),
        };
        let join_json = serde_json::to_string(&join_msg).unwrap();
        sender
            .send(Message::Text(join_json))
            .map_err(|_| ChatServiceError::SendError)?;

        Ok((ChatClient { sender }, msg_receiver))
    }

    pub async fn send_request(&self, msg: ServerMessage) -> Result<(), ChatServiceError> {
        self.send_message_to_server(msg)?;
        Ok(())
    }

    pub async fn handle_response(&self) -> Result<(), ChatServiceError> {
        Ok(())
    }

    pub fn send_message_to_server(&self, msg: ServerMessage) -> Result<(), ChatServiceError> {
        let msg_json = serde_json::to_string(&msg).unwrap();
        // println!("Message to sender: {}", msg_json);
        self.sender
            .send(Message::Text(msg_json))
            .map_err(|_| ChatServiceError::SendError)?;
        Ok(())
    }
}

#[test]
fn test_sign() {
    use alloy::signers::SignerSync;
    let signer = alloy::signers::local::PrivateKeySigner::from_str(
        "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    )
    .unwrap();

    // Sign a message.
    let message = "You are joining the group with smart contract: ".to_owned()
        + "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512";
    let signature = signer.sign_message_sync(message.as_bytes()).unwrap();

    let json = serde_json::to_string(&signature).unwrap();
    let recover_sig: Signature = serde_json::from_str(&json).unwrap();

    // Recover the signer from the message.
    let recovered = recover_sig.recover_address_from_msg(message).unwrap();
    assert_eq!(recovered, signer.address());
}

#[test]
fn json_test() {
    let inner_msg = ChatMessages::Request(RequestMLSPayload::new(
        "sc_address".to_string(),
        ReqMessageType::InviteToGroup,
    ));

    let res = serde_json::to_string(&inner_msg);
    assert!(res.is_ok());
    let json_inner_msg = res.unwrap();

    let server_msg = ServerMessage::InMessage {
        from: "alice".to_string(),
        to: vec!["bob".to_string()],
        msg: json_inner_msg,
    };

    let res = serde_json::to_string(&server_msg);
    assert!(res.is_ok());
    let json_server_msg = res.unwrap();

    ////

    if let Ok(chat_message) = serde_json::from_str::<ServerMessage>(&json_server_msg) {
        println!("Server: {:?}", chat_message);
        match chat_message {
            ServerMessage::InMessage { from, to, msg } => {
                println!("Chat: {:?}", msg);
                if let Ok(chat_msg) = serde_json::from_str::<ChatMessages>(&msg) {
                    match chat_msg {
                        ChatMessages::Request(req) => println!("Request: {:?}", req),
                        ChatMessages::Response(_) => println!("Response"),
                        ChatMessages::Welcome(_) => println!("Welcome"),
                    }
                }
            }
            ServerMessage::SystemJoin { username } => println!("SystemJoin"),
        }
    }
}
