use axum::extract::ws::{Message as WsMessage, WebSocket};
use futures::{stream::SplitSink, SinkExt};
use kameo::{
    message::{Context, Message},
    Actor,
};
use log::info;
use serde_json::Value;

use crate::{
    message::{ConnectMessage, UserMessage},
    protos::messages::v1::{app_message, AppMessage},
};

/// This actor is used to handle messages from web socket
#[derive(Debug, Actor)]
pub struct WsActor {
    /// This is the sender of the open web socket connection
    pub ws_sender: SplitSink<WebSocket, WsMessage>,
    /// This variable is used to check if the user has connected to the ws,
    ///   if not, we parse message as ConnectMessage
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
/// DoNothing - this action is used for test purposes (return empty action if message is not valid)
#[derive(Debug, PartialEq)]
pub enum WsAction {
    Connect(ConnectMessage),
    UserMessage(UserMessage),
    RemoveUser(String, String),
    UserVote {
        proposal_id: u32,
        vote: bool,
        group_id: String,
    },
    DoNothing,
}

/// This struct is used to represent the raw message from the web socket.
/// It is used to handle the message from the web socket and return it to the user
/// We can parse it to the ConnectMessage or UserMessage
#[derive(Debug, PartialEq)]
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
        match serde_json::from_str::<Value>(&msg.message) {
            Ok(json_data) => {
                // Handle different JSON message types
                if let Some(type_field) = json_data.get("type") {
                    if let Some("user_vote") = type_field.as_str() {
                        if let (Some(proposal_id), Some(vote), Some(group_id)) = (
                            json_data.get("proposal_id").and_then(|v| v.as_u64()),
                            json_data.get("vote").and_then(|v| v.as_bool()),
                            json_data.get("group_id").and_then(|v| v.as_str()),
                        ) {
                            return Ok(WsAction::UserVote {
                                proposal_id: proposal_id as u32,
                                vote,
                                group_id: group_id.to_string(),
                            });
                        }
                    }
                }

                // Check if it's a UserMessage format
                if let (Some(message), Some(group_id)) = (
                    json_data.get("message").and_then(|v| v.as_str()),
                    json_data.get("group_id").and_then(|v| v.as_str()),
                ) {
                    // Handle commands
                    if message.starts_with("/") {
                        let mut tokens = message.split_whitespace();
                        match tokens.next() {
                            Some("/ban") => {
                                let user_to_ban = tokens.next();
                                if let Some(user_to_ban) = user_to_ban {
                                    let user_to_ban = user_to_ban.to_lowercase();
                                    return Ok(WsAction::RemoveUser(
                                        user_to_ban.to_string(),
                                        group_id.to_string(),
                                    ));
                                } else {
                                    return Err(WsError::InvalidMessage);
                                }
                            }
                            _ => return Err(WsError::InvalidMessage),
                        }
                    }

                    return Ok(WsAction::UserMessage(UserMessage {
                        message: message.as_bytes().to_vec(),
                        group_id: group_id.to_string(),
                    }));
                }

                Err(WsError::InvalidMessage)
            }
            Err(_) => {
                // Try to parse as UserMessage as fallback
                match serde_json::from_str::<UserMessage>(&msg.message) {
                    Ok(user_msg) => Ok(WsAction::UserMessage(user_msg)),
                    Err(_) => Err(WsError::InvalidMessage),
                }
            }
        }
    }
}

/// This impl is used to send messages to the websocket
impl Message<AppMessage> for WsActor {
    type Reply = Result<(), WsError>;

    async fn handle(
        &mut self,
        msg: AppMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        // Check if this is a voting proposal and format it specially for the frontend
        let message_text =
            if let Some(app_message::Payload::VotingProposal(voting_proposal)) = &msg.payload {
                // Format as JSON for the frontend to parse
                info!("[ws_actor::handle]: Sending voting proposal to ws");
                serde_json::json!({
                    "type": "voting_proposal",
                    "proposal": {
                        "proposal_id": voting_proposal.proposal_id,
                        "group_name": voting_proposal.group_name,
                        "payload": voting_proposal.payload
                    }
                })
                .to_string()
            } else {
                msg.to_string()
            };
        self.ws_sender.send(WsMessage::Text(message_text)).await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WsError {
    #[error("Invalid message")]
    InvalidMessage,
    #[error("Malformed json: {0}")]
    MalformedJson(#[from] serde_json::Error),
    #[error("Failed to send message to websocket: {0}")]
    SendMessageError(#[from] axum::Error),
}
