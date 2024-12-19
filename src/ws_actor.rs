use std::fmt::Display;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{stream::SplitSink, SinkExt};
use kameo::{
    actor::ActorRef,
    message::{Context, Message},
    Actor,
};
use serde::Deserialize;

#[derive(Debug, Actor)]
pub struct WsActor {
    pub ws_sender: SplitSink<WebSocket, WsMessage>,
    pub is_initialized: bool,
}

pub enum WsAction {
    Connect(ConnectMessage),
    UserMessage(UserMessage),
    DoNothing,
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
        ctx: Context<'_, Self, Self::Reply>,
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

pub struct WsChatMessage {
    pub message: String,
    pub sender: String,
}

impl Display for WsChatMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.sender, self.message)
    }
}

impl Message<WsChatMessage> for WsActor {
    type Reply = Result<(), WsError>;

    async fn handle(&mut self, msg: WsChatMessage, ctx: Context<'_, Self, Self::Reply>) -> Self::Reply {
        self.ws_sender.send(WsMessage::Text(msg.to_string())).await?;
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
