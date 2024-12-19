use alloy::signers::local::LocalSignerError;
use ecies::{decrypt, encrypt};
use kameo::{actor::ActorRef, error::SendError};
use libsecp256k1::{sign, verify, Message, PublicKey, SecretKey, Signature as libSignature};
use openmls::{error::LibraryError, prelude::*};
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
use waku_bindings::{WakuContentTopic, WakuMessage};

use ds::{
    waku_actor::{
        ProcessMessageToSend, ProcessSubscribeToGroup, ProcessUnsubscribeFromGroup, WakuActor,
    },
    DeliveryServiceError,
};
use sc_key_store::KeyStoreError;

pub mod group_actor;
pub mod identity;
pub mod main_loop;
pub mod user;
pub mod ws_actor;

pub struct AppState {
    pub waku_actor: ActorRef<WakuActor>,
    pub rooms: Mutex<HashSet<String>>,
    pub app_id: Vec<u8>,
    pub content_topics: Arc<Mutex<Vec<WakuContentTopic>>>,
    pub pubsub: tokio::sync::broadcast::Sender<WakuMessage>,
}

pub fn group_id_to_name(group_id: Vec<u8>) -> String {
    String::from_utf8(group_id).unwrap()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WelcomeMessageType {
    GroupAnnouncement,
    KeyPackageShare,
    WelcomeShare,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMessage {
    pub sender: Vec<u8>,
    pub message: Vec<u8>,
}

impl AppMessage {
    pub fn new(sender: Vec<u8>, message: Vec<u8>) -> Self {
        AppMessage { sender, message }
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
pub enum IdentityError {
    #[error("Failed to create new key package: {0}")]
    MlsKeyPackageCreationError(#[from] KeyPackageNewError<MemoryKeyStoreError>),
    #[error(transparent)]
    MlsLibraryError(#[from] LibraryError),
    #[error("Failed to create signature: {0}")]
    MlsCryptoError(#[from] CryptoError),
    #[error("Failed to save signature key: {0}")]
    MlsKeyStoreError(#[from] MemoryKeyStoreError),
    #[error("Failed to create credential: {0}")]
    MlsCredentialError(#[from] CredentialError),
    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("Admin not set")]
    AdminNotSetError,
    #[error(transparent)]
    AdminError(#[from] AdminError),
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
    #[error("Failed to create staged join: {0}")]
    MlsWelcomeError(#[from] WelcomeError<MemoryKeyStoreError>),
    #[error("Failed to validate user key package: {0}")]
    MlsKeyPackageVerificationError(#[from] KeyPackageVerifyError),
    #[error("Failed to remove members: {0}")]
    MlsRemoveMembersError(#[from] RemoveMembersError<MemoryKeyStoreError>),
    #[error("Failed to leave group: {0}")]
    MlsLeaveGroupError(#[from] LeaveGroupError),

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
    #[error("Group still active")]
    GroupStillActiveError,

    #[error("Unsupported message type.")]
    UnsupportedMessageType,
    #[error("Unknown content topic type: {0}")]
    UnknownContentTopicType(String),
    #[error("Welcome message cannot be empty.")]
    EmptyWelcomeMessageError,
    #[error("Message verification failed")]
    MessageVerificationFailed,
}

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("Admin not found: {0}")]
    AdminNotFoundError(String),
    #[error("Admin already exists: {0}")]
    AdminAlreadyExistsError(String),
    #[error(transparent)]
    MessageError(#[from] MessageError),
    #[error("JSON processing error: {0}")]
    JsonError(#[from] serde_json::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum MessageError {
    #[error("Message verification failed")]
    MessageVerificationFailed,
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
    KeyStoreError(#[from] KeyStoreError),
    #[error(transparent)]
    IdentityError(#[from] IdentityError),
    #[error(transparent)]
    GroupError(#[from] GroupError),
    #[error(transparent)]
    AdminError(#[from] AdminError),
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
    #[error("Failed to create staged join: {0}")]
    MlsWelcomeError(#[from] WelcomeError<MemoryKeyStoreError>),
    #[error("Failed to validate user key package: {0}")]
    MlsKeyPackageVerificationError(#[from] KeyPackageVerifyError),

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

    #[error("Failed to subscribe to group: {0}")]
    KameoSubscribeToGroupError(#[from] SendError<ProcessSubscribeToGroup, DeliveryServiceError>),
    #[error("Failed to publish message: {0}")]
    KameoPublishMessageError(#[from] SendError<ProcessMessageToSend, DeliveryServiceError>),
    #[error("Failed to unsubscribe from group: {0}")]
    KameoUnsubscribeFromGroupError(
        #[from] SendError<ProcessUnsubscribeFromGroup, DeliveryServiceError>,
    ),
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
