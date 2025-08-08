//! This module contains the messages that are used to communicate between inside the application
//! The high level message is a [`WakuMessage`](waku_bindings::WakuMessage)
//! Inside the [`WakuMessage`](waku_bindings::WakuMessage) we have a [`ContentTopic`](waku_bindings::WakuContentTopic) and a payload
//! The [`ContentTopic`](waku_bindings::WakuContentTopic) is used to identify the type of message and the payload is the actual message
//! Based on the [`ContentTopic`](waku_bindings::WakuContentTopic) we distinguish between:
//!  - [`WelcomeMessage`] which includes next message types:
//!    - [`GroupAnnouncement`]
//!         - `GroupAnnouncement {
//!             eth_pub_key: Vec<u8>,
//!             signature: Vec<u8>,
//!           }`
//!    - [`UserKeyPackage`]
//!         - `Encrypted KeyPackage: Vec<u8>`
//!    - [`InvitationToJoin`]
//!         - `Serialized MlsMessageOut: Vec<u8>`
//!  - [`AppMessage`]
//!    - [`ConversationMessage`]
//!    - [`BatchProposalsMessage`]
//!

use crate::{
    encrypt_message,
    protos::messages::v1::{app_message, UserKeyPackage},
    verify_message, MessageError,
};
// use log::info;
use openmls::prelude::{KeyPackage, MlsMessageOut};
use serde::{Deserialize, Serialize};
use std::fmt::Display;

use crate::protos::messages::v1::{
    welcome_message, AppMessage, BanRequest, BatchProposalsMessage, ConversationMessage,
    GroupAnnouncement, InvitationToJoin, WelcomeMessage,
};

// WELCOME MESSAGE SUBTOPIC

pub fn wrap_group_announcement_in_welcome_msg(
    group_announcement: GroupAnnouncement,
) -> WelcomeMessage {
    WelcomeMessage {
        payload: Some(welcome_message::Payload::GroupAnnouncement(
            group_announcement,
        )),
    }
}

pub fn wrap_user_kp_into_welcome_msg(
    encrypted_kp: Vec<u8>,
) -> Result<WelcomeMessage, MessageError> {
    let user_key_package = UserKeyPackage {
        encrypt_kp: encrypted_kp,
    };
    let welcome_message = WelcomeMessage {
        payload: Some(welcome_message::Payload::UserKeyPackage(user_key_package)),
    };
    Ok(welcome_message)
}
pub fn wrap_invitation_into_welcome_msg(
    mls_message: MlsMessageOut,
) -> Result<WelcomeMessage, MessageError> {
    let mls_bytes = mls_message.to_bytes()?;
    let invitation = InvitationToJoin {
        mls_message_out_bytes: mls_bytes,
    };

    let welcome_message = WelcomeMessage {
        payload: Some(welcome_message::Payload::InvitationToJoin(invitation)),
    };
    Ok(welcome_message)
}

impl GroupAnnouncement {
    pub fn new(pub_key: Vec<u8>, signature: Vec<u8>) -> Self {
        GroupAnnouncement {
            eth_pub_key: pub_key,
            signature,
        }
    }

    pub fn verify(&self) -> Result<bool, MessageError> {
        let verified = verify_message(&self.eth_pub_key, &self.signature, &self.eth_pub_key)?;
        Ok(verified)
    }

    pub fn encrypt(&self, kp: KeyPackage) -> Result<Vec<u8>, MessageError> {
        // TODO: replace json in encryption and decryption
        let key_package = serde_json::to_vec(&kp)?;
        let encrypted = encrypt_message(&key_package, &self.eth_pub_key)?;
        Ok(encrypted)
    }
}

// APPLICATION MESSAGE SUBTOPIC

pub fn wrap_conversation_message_into_application_msg(
    message: Vec<u8>,
    sender: String,
    group_name: String,
) -> AppMessage {
    AppMessage {
        payload: Some(app_message::Payload::ConversationMessage(
            ConversationMessage {
                message,
                sender,
                group_name,
            },
        )),
    }
}

pub fn wrap_batch_proposals_into_application_msg(
    group_name: String,
    mls_proposals: Vec<Vec<u8>>,
    commit_message: Vec<u8>,
) -> AppMessage {
    AppMessage {
        payload: Some(app_message::Payload::BatchProposalsMessage(
            BatchProposalsMessage {
                group_name: group_name.into_bytes(),
                mls_proposals,
                commit_message,
            },
        )),
    }
}

pub fn wrap_ban_request_into_application_msg(
    user_to_ban: String,
    requester: String,
    group_name: String,
) -> AppMessage {
    AppMessage {
        payload: Some(app_message::Payload::BanRequest(BanRequest {
            user_to_ban: user_to_ban.into(),
            requester,
            group_name,
        })),
    }
}

impl Display for AppMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.payload {
            Some(app_message::Payload::ConversationMessage(conversation_message)) => {
                write!(
                    f,
                    "{}: {}",
                    conversation_message.sender,
                    String::from_utf8_lossy(&conversation_message.message)
                )
            }
            _ => write!(f, "Invalid message"),
        }
    }
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
