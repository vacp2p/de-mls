use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{stream::SplitSink, SinkExt};
use kameo::{
    message::{Context, Message},
    Actor,
};
use serde::Deserialize;

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

pub enum WsAction {
    Connect(ConnectMessage),
    UserMessage(UserMessage),
}

#[derive(Deserialize, Debug)]
pub struct UserMessage {
    pub message: String,
    pub group_id: String,
}

#[derive(Deserialize, Debug)]
pub struct ConnectMessage {
    pub eth_private_key: String,
    pub group_id: String,
    pub should_create: bool,
}

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
