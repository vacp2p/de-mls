use alloy::primitives::Address;
use alloy::signers::Signature;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::chat_server::ServerMessage;
use crate::DeliveryServiceError;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum ChatMessages {
    Request(RequestMLSPayload),
    Response(ResponseMLSPayload),
    Welcome(String),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub enum ReqMessageType {
    InviteToGroup,
    RemoveFromGroup,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct RequestMLSPayload {
    sc_address: String,
    group_name: String,
    pub msg_type: ReqMessageType,
}

impl RequestMLSPayload {
    pub fn new(sc_address: String, group_name: String, msg_type: ReqMessageType) -> Self {
        RequestMLSPayload {
            sc_address,
            group_name,
            msg_type,
        }
    }

    pub fn msg_to_sign(&self) -> String {
        self.sc_address.to_owned() + &self.group_name
    }

    pub fn group_name(&self) -> String {
        self.group_name.clone()
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ResponseMLSPayload {
    signature: String,
    user_address: String,
    pub group_name: String,
    key_package: Vec<u8>,
}

impl ResponseMLSPayload {
    pub fn new(
        signature: String,
        user_address: String,
        group_name: String,
        key_package: Vec<u8>,
    ) -> Self {
        Self {
            signature,
            user_address,
            group_name,
            key_package,
        }
    }

    pub fn validate(
        &self,
        sc_address: String,
        group_name: String,
    ) -> Result<(String, Vec<u8>), DeliveryServiceError> {
        let recover_sig: Signature = serde_json::from_str(&self.signature)?;
        let addr = Address::from_str(&self.user_address)?;
        // Recover the signer from the message.
        let recovered =
            recover_sig.recover_address_from_msg(sc_address.to_owned() + &group_name)?;

        if recovered.ne(&addr) {
            return Err(DeliveryServiceError::ValidationError(recovered.to_string()));
        }
        Ok((self.user_address.clone(), self.key_package.clone()))
    }
}

pub struct ChatClient {
    sender: mpsc::UnboundedSender<Message>,
}

impl ChatClient {
    pub async fn connect(
        server_addr: &str,
        username: String,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Message>), DeliveryServiceError> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(server_addr).await?;
        let (mut write, read) = ws_stream.split();
        let (sender, receiver) = mpsc::unbounded_channel();
        let (msg_sender, msg_receiver) = mpsc::unbounded_channel();

        let receiver = Arc::new(Mutex::new(receiver));

        // Spawn a task to handle outgoing messages
        tokio::spawn(async move {
            while let Some(message) = receiver.lock().await.recv().await {
                if let Err(err) = write.send(message).await {
                    return Err(DeliveryServiceError::SenderError(err.to_string()));
                }
            }
            Ok(())
        });

        // Spawn a task to handle incoming messages
        tokio::spawn(async move {
            let mut read = read;
            while let Some(Ok(message)) = read.next().await {
                if let Err(err) = msg_sender.send(message) {
                    return Err(DeliveryServiceError::SenderError(err.to_string()));
                }
            }
            Ok(())
        });

        // Send a SystemJoin message when registering
        let join_msg = ServerMessage::SystemJoin {
            username: username.to_string(),
        };
        let join_json = serde_json::to_string(&join_msg).unwrap();
        sender
            .send(Message::Text(join_json))
            .map_err(|err| DeliveryServiceError::SenderError(err.to_string()))?;

        Ok((ChatClient { sender }, msg_receiver))
    }

    pub fn send_message(&self, msg: ServerMessage) -> Result<(), DeliveryServiceError> {
        let msg_json = serde_json::to_string(&msg).unwrap();
        self.sender
            .send(Message::Text(msg_json))
            .map_err(|err| DeliveryServiceError::SenderError(err.to_string()))?;
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
        "group_name".to_string(),
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
        assert_eq!(chat_message, server_msg);
        match chat_message {
            ServerMessage::InMessage { from, to, msg } => {
                if let Ok(chat_msg) = serde_json::from_str::<ChatMessages>(&msg) {
                    assert_eq!(chat_msg, inner_msg);
                }
            }
            ServerMessage::SystemJoin { username } => {}
        }
    }
}
