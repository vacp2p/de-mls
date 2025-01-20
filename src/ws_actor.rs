use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{stream::SplitSink, SinkExt};
use kameo::{
    message::{Context, Message},
    Actor,
};
use serde::{Deserialize, Serialize};

use crate::MessageToPrint;

/// This actor is used to handle messages from web socket
#[derive(Debug, Actor)]
pub struct WsActor {
    /// This is the sender of the open web socket connection
    pub ws_sender: SplitSink<WebSocket, WsMessage>,
    /// This variable is used to check if the user has connected to the ws, if not, we parce message as ConnectMessage
    pub is_initialized: bool,
}

impl WsActor {
    pub fn new(ws_sender: SplitSink<WebSocket, WsMessage>) -> Self {
        Self {
            ws_sender,
            is_initialized: false,
        }
    }
}

/// This enum is used to represent the actions that can be performed on the web socket
/// Connect - this action is used to return connection data to the user
/// UserMessage - this action is used to handle message from web socket and return it to the user
/// RemoveUser - this action is used to remove a user from the group
/// DoNothing - this action is used for test purposes (return empty action if message is not valid)
#[derive(Debug, PartialEq)]
pub enum WsAction {
    Connect(ConnectMessage),
    UserMessage(UserMessage),
    RemoveUser(String, String),
    DoNothing,
}

/// This struct is used to represent the message from the user that we got from web socket
#[derive(Deserialize, Debug, PartialEq, Serialize)]
pub struct UserMessage {
    pub message: String,
    pub group_id: String,
}

/// This struct is used to represent the connection data that web socket sends to the user
#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct ConnectMessage {
    /// This is the private key of the user that we will use to authenticate the user
    pub eth_private_key: String,
    /// This is the id of the group that the user is joining
    pub group_id: String,
    /// This is the flag that indicates if the user should create a new group or subscribe to an existing one
    pub should_create: bool,
}

/// This struct is used to represent the raw message from the web socket.
/// It is used to handle the message from the web socket and return it to the user
/// We can parse it to the ConnectMessage or UserMessage
///     if it starts with "/ban" it will be parsed to RemoveUser, otherwise it will be parsed to UserMessage
#[derive(Deserialize, Debug, PartialEq)]
pub struct RawWsMessage {
    pub message: String,
}

impl Message<RawWsMessage> for WsActor {
    type Reply = Result<WsAction, WsError>;

    async fn handle(
        &mut self,
        msg: RawWsMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        if !self.is_initialized {
            let connect_message = serde_json::from_str(&msg.message)?;
            self.is_initialized = true;
            return Ok(WsAction::Connect(connect_message));
        }
        match serde_json::from_str(&msg.message) {
            Ok(UserMessage { message, group_id }) => {
                if message.starts_with("/") {
                    let mut tokens = message.split_whitespace();
                    match tokens.next() {
                        Some("/ban") => {
                            let user_to_ban = tokens.next();
                            if user_to_ban.is_none() {
                                return Err(WsError::InvalidMessage);
                            } else {
                                let user_to_ban = user_to_ban.unwrap().to_lowercase();
                                return Ok(WsAction::RemoveUser(
                                    user_to_ban.to_string(),
                                    group_id.clone(),
                                ));
                            }
                        }
                        _ => return Err(WsError::InvalidMessage),
                    }
                }
                Ok(WsAction::UserMessage(UserMessage { message, group_id }))
            }
            Err(_) => Err(WsError::InvalidMessage),
        }
    }
}

/// This impl is used to send messages to the websocket
impl Message<MessageToPrint> for WsActor {
    type Reply = Result<(), WsError>;

    async fn handle(
        &mut self,
        msg: MessageToPrint,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.ws_sender
            .send(WsMessage::Text(msg.to_string()))
            .await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("Invalid message")]
    InvalidMessage,
    #[error("Malformed json")]
    MalformedJson(#[from] serde_json::Error),
    #[error("Failed to send message")]
    SendMessageError(#[from] axum::Error),
}
