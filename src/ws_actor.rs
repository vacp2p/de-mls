use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{stream::SplitSink, SinkExt};
use kameo::{
    message::{Context, Message},
    Actor,
};
use log::info;
use serde::{Deserialize, Serialize};

use crate::MessageToPrint;

#[derive(Debug, Actor)]
pub struct WsActor {
    pub ws_sender: SplitSink<WebSocket, WsMessage>,
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

#[derive(Debug, PartialEq)]
pub enum WsAction {
    Connect(ConnectMessage),
    UserMessage(UserMessage),
    RemoveUser(String, String),
    DoNothing,
}

#[derive(Deserialize, Debug, PartialEq, Serialize)]
pub struct UserMessage {
    pub message: String,
    pub group_id: String,
}

#[derive(Deserialize, Debug, PartialEq)]
pub struct ConnectMessage {
    pub eth_private_key: String,
    pub group_id: String,
    pub should_create: bool,
}

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
        info!("Got message from ws: {:?}", &msg);
        match serde_json::from_str(&msg.message) {
            Ok(UserMessage { message, group_id }) => {
                info!("Got user message: {:?}", &message);
                if message.starts_with("/") {
                    let mut tokens = message.split_whitespace();
                    match tokens.next() {
                        Some("/ban") => {
                            let user_to_ban = tokens.next().unwrap().to_lowercase();
                            return Ok(WsAction::RemoveUser(
                                user_to_ban.to_string(),
                                group_id.clone(),
                            ));
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
