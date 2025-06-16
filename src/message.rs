//! This module contains the messages that are used to communicate between inside the application
//! The high level message is a [`WakuMessage`](waku_bindings::WakuMessage)
//! Inside the [`WakuMessage`](waku_bindings::WakuMessage) we have a [`ContentTopic`](waku_bindings::WakuContentTopic) and a payload
//! The [`ContentTopic`](waku_bindings::WakuContentTopic) is used to identify the type of message and the payload is the actual message
//! Based on the [`ContentTopic`](waku_bindings::WakuContentTopic) we distinguish between:
//!  - [`WelcomeMessage`](crate::message::WelcomeMessage) which includes next message types:
//!    - [`GroupAnnouncement`](crate::message::WelcomeMessageType::GroupAnnouncement)
//!         - GroupAnnouncement {
//!             eth_pub_key: Vec<u8>,
//!             signature: Vec<u8>,
//!           }
//!    - [`UserKeyPackage`](crate::message::WelcomeMessageType::UserKeyPackage)
//!         - Encrypted KeyPackage: Vec<u8>
//!    - [`InvitationToJoin`](crate::message::WelcomeMessageType::InvitationToJoin)
//!         - Serialized MlsMessageOut: Vec<u8>
//!  - [`ProtocolMessage`](crate::message::ProtocolMessage)
//!    
//!
//!
//! Next level of abstraction is the [ProtocolMessage](openmls::group::ProtocolMessage)
//! This is the message that is used to communicate between the group members
//! It contains the group id, the message type, the message payload and the signature
//! The message type is used to identify the type of message and the payload is the actual message
//! The signature is used to verify the message
//!
//! The [ProtocolMessage] is used to communicate between the group members

use crate::{
    encrypt_message,
    protos::messages::v1::{app_message, UserKeyPackage, VoteStartMessage},
    verify_message, MessageError,
};
// use log::info;
use openmls::prelude::{KeyPackage, MlsMessageOut};
use std::fmt::Display;

// use crate::protos::messages::v1::{
//     welcome_message, GroupAnnouncement, InvitationToJoin, WelcomeMessage, AppMessage, ConversationMessage, UserKeyPackage,
// };
use crate::protos::messages::v1::{
    welcome_message, AppMessage, ConversationMessage, GroupAnnouncement, InvitationToJoin,
    WelcomeMessage,
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
    println!(
        "Start wrapping invitation into welcome message: {:?}",
        mls_message.body()
    );
    let mls_bytes = mls_message.to_bytes()?;
    let invitation = InvitationToJoin {
        mls_message_out_bytes: mls_bytes,
    };

    let welcome_message = WelcomeMessage {
        payload: Some(welcome_message::Payload::InvitationToJoin(invitation)),
    };
    println!("End wrapping invitation into welcome message");
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

pub fn wrap_vote_start_message_into_application_msg(group_name: String) -> AppMessage {
    AppMessage {
        payload: Some(app_message::Payload::VoteStartMessage(VoteStartMessage {
            group_name: group_name.into_bytes(),
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
