use alloy::signers::local::LocalSignerError;
use ecies::{decrypt, encrypt};
use kameo::error::SendError;
use libsecp256k1::{sign, verify, Message, PublicKey, SecretKey, Signature as libSignature};
use openmls::prelude::*;
use openmls_rust_crypto::MemoryKeyStoreError;
use rand::thread_rng;
use secp256k1::hashes::{sha256, Hash};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fmt::Display,
    str::Utf8Error,
    string::FromUtf8Error,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::Sender;
use waku_bindings::{WakuContentTopic, WakuMessage};

use ds::{waku_actor::WakuMessageToSend, DeliveryServiceError};
use mls_crypto::IdentityError;

pub mod action_handlers;
pub mod admin;
pub mod group;
pub mod user;
pub mod user_app_instance;
pub mod ws_actor;

pub struct AppState {
    pub waku_node: Sender<WakuMessageToSend>,
    pub rooms: Mutex<HashSet<String>>,
    pub content_topics: Arc<Mutex<Vec<WakuContentTopic>>>,
    pub pubsub: tokio::sync::broadcast::Sender<WakuMessage>,
}

#[derive(Debug, Clone)]
pub struct Connection {
    pub eth_private_key: String,
    pub group_id: String,
    pub should_create_group: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WelcomeMessageType {
    GroupAnnouncement,
    UserKeyPackage,
    InvitationToJoin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeMessage {
    pub message_type: WelcomeMessageType,
    pub message_payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupAnnouncement {
    pub_key: Vec<u8>,
    signature: Vec<u8>,
}

impl GroupAnnouncement {
    pub fn new(pub_key: Vec<u8>, signature: Vec<u8>) -> Self {
        GroupAnnouncement { pub_key, signature }
    }

    pub fn verify(&self) -> Result<bool, MessageError> {
        let verified = verify_message(&self.pub_key, &self.signature, &self.pub_key)?;
        Ok(verified)
    }

    pub fn encrypt(&self, data: Vec<u8>) -> Result<Vec<u8>, MessageError> {
        let encrypted = encrypt_message(&data, &self.pub_key)?;
        Ok(encrypted)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageToPrint {
    pub sender: String,
    pub message: String,
    pub group_name: String,
}

impl MessageToPrint {
    pub fn new(sender: String, message: String, group_name: String) -> Self {
        MessageToPrint {
            sender,
            message,
            group_name,
        }
    }
}

impl Display for MessageToPrint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.sender, self.message)
    }
}

pub fn generate_keypair() -> (PublicKey, SecretKey) {
    let secret_key = SecretKey::random(&mut thread_rng());
    let public_key = PublicKey::from_secret_key(&secret_key);
    (public_key, secret_key)
}

pub fn sign_message(message: &[u8], secret_key: &SecretKey) -> Vec<u8> {
    let digest = sha256::Hash::hash(message);
    let msg = Message::parse(&digest.to_byte_array());
    let signature = sign(&msg, secret_key);
    signature.0.serialize_der().as_ref().to_vec()
}

pub fn verify_message(
    message: &[u8],
    signature: &[u8],
    public_key: &[u8],
) -> Result<bool, MessageError> {
    let digest = sha256::Hash::hash(message);
    let msg = Message::parse(&digest.to_byte_array());
    let signature = libSignature::parse_der(signature)?;
    let mut pub_key_bytes: [u8; 33] = [0; 33];
    pub_key_bytes[..].copy_from_slice(public_key);
    let public_key = PublicKey::parse_compressed(&pub_key_bytes)?;
    Ok(verify(&msg, &signature, &public_key))
}

pub fn encrypt_message(message: &[u8], public_key: &[u8]) -> Result<Vec<u8>, MessageError> {
    let encrypted = encrypt(public_key, message)?;
    Ok(encrypted)
}

pub fn decrypt_message(message: &[u8], secret_key: SecretKey) -> Result<Vec<u8>, MessageError> {
    let secret_key_serialized = secret_key.serialize();
    let decrypted = decrypt(&secret_key_serialized, message)?;
    Ok(decrypted)
}

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("Admin not set")]
    AdminNotSetError,
    #[error(transparent)]
    MessageError(#[from] MessageError),
    #[error("MLS group not initialized")]
    MlsGroupNotInitializedError,

    #[error("Error while creating MLS group: {0}")]
    MlsGroupCreationError(#[from] NewGroupError<MemoryKeyStoreError>),
    #[error("Error while adding member to MLS group: {0}")]
    MlsAddMemberError(#[from] AddMembersError<MemoryKeyStoreError>),
    #[error("Error while merging pending commit in MLS group: {0}")]
    MlsMergePendingCommitError(#[from] MergePendingCommitError<MemoryKeyStoreError>),
    #[error("Error while merging commit in MLS group: {0}")]
    MlsMergeCommitError(#[from] MergeCommitError<MemoryKeyStoreError>),
    #[error("Error processing unverified message: {0}")]
    MlsProcessMessageError(#[from] ProcessMessageError),
    #[error("Error while creating message: {0}")]
    MlsCreateMessageError(#[from] CreateMessageError),
    #[error("Failed to remove members: {0}")]
    MlsRemoveMembersError(#[from] RemoveMembersError<MemoryKeyStoreError>),
    #[error("Group still active")]
    GroupStillActiveError,

    #[error("UTF-8 parsing error: {0}")]
    Utf8ParsingError(#[from] FromUtf8Error),
    #[error("JSON processing error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] tls_codec::Error),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("Failed to verify signature: {0}")]
    SignatureVerificationError(#[from] libsecp256k1::Error),
    #[error("JSON processing error: {0}")]
    JsonError(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error(transparent)]
    DeliveryServiceError(#[from] DeliveryServiceError),
    #[error(transparent)]
    IdentityError(#[from] IdentityError),
    #[error(transparent)]
    GroupError(#[from] GroupError),
    #[error(transparent)]
    MessageError(#[from] MessageError),

    #[error("Group already exists: {0}")]
    GroupAlreadyExistsError(String),
    #[error("Group not found: {0}")]
    GroupNotFoundError(String),

    #[error("Unsupported message type.")]
    UnsupportedMessageType,
    #[error("Welcome message cannot be empty.")]
    EmptyWelcomeMessageError,
    #[error("Message verification failed")]
    MessageVerificationFailed,

    #[error("Unknown content topic type: {0}")]
    UnknownContentTopicType(String),

    #[error("Failed to create staged join: {0}")]
    MlsWelcomeError(#[from] WelcomeError<MemoryKeyStoreError>),

    #[error("UTF-8 parsing error: {0}")]
    Utf8ParsingError(#[from] FromUtf8Error),
    #[error("UTF-8 string parsing error: {0}")]
    Utf8StringParsingError(#[from] Utf8Error),
    #[error("JSON processing error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Serialization error: {0}")]
    SerializationError(#[from] tls_codec::Error),
    #[error("Failed to parse signer: {0}")]
    SignerParsingError(#[from] LocalSignerError),

    #[error("Failed to publish message: {0}")]
    KameoPublishMessageError(#[from] SendError<WakuMessageToSend, DeliveryServiceError>),
    #[error("Failed to create group: {0}")]
    KameoCreateGroupError(String),
    #[error("Failed to send message to user: {0}")]
    KameoSendMessageError(String),

    #[error("Failed to get income key packages: {0}")]
    GetIncomeKeyPackagesError(String),
    #[error("Failed to process admin message: {0}")]
    ProcessAdminMessageError(String),
    #[error("Failed to process invite users: {0}")]
    ProcessInviteUsersError(String),

    #[error("Failed to send message to waku: {0}")]
    WakuSendMessageError(#[from] tokio::sync::mpsc::error::SendError<WakuMessageToSend>),
}

/// Check if a content topic exists in a list of topics or if the list is empty
pub fn match_content_topic(
    content_topics: &Arc<Mutex<Vec<WakuContentTopic>>>,
    topic: &WakuContentTopic,
) -> bool {
    let locked_topics = content_topics.lock().unwrap();
    locked_topics.is_empty() || locked_topics.iter().any(|t| t == topic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_message() {
        let message = b"Hello, world!";
        let (public_key, secret_key) = generate_keypair();
        let signature = sign_message(message, &secret_key);
        let verified = verify_message(message, &signature, &public_key.serialize_compressed());
        assert!(verified.is_ok());
        assert!(verified.unwrap());
    }

    #[test]
    fn test_encrypt_decrypt_message() {
        let message = b"Hello, world!";
        let (public_key, secret_key) = generate_keypair();
        let encrypted = encrypt_message(message, &public_key.serialize_compressed());
        let decrypted = decrypt_message(&encrypted.unwrap(), secret_key);
        assert_eq!(message, decrypted.unwrap().as_slice());
    }
}
