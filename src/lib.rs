//! # DE-MLS: Distributed MLS Group Management System
//!
//! This crate provides a distributed group management system built on top of MLS (Message Layer Security).
//! It implements a steward-based epoch management system with HashGraph-like consensus for secure group operations.
//!
//! ## Architecture Overview
//!
//! The system consists of several key components:
//!
//! ### Core Components
//!
//! - **Group Management** (`group.rs`): Orchestrates MLS group operations and state transitions
//! - **State Machine** (`state_machine.rs`): Manages steward epoch states and transitions
//! - **Steward** (`steward.rs`): Handles proposal collection and management
//! - **Consensus** (`consensus/`): Provides distributed consensus for voting based on HashGraph-like protocol
//! - **User Management** (`user.rs`): Manages individual user operations and message handling
//!
//! ### Actor System
//!
//! - **User Actor** (`user_actor.rs`): Actor-based user management with message handling
//! - **WebSocket Actor** (`ws_actor.rs`): Handles WebSocket connections and message routing
//! - **Action Handlers** (`action_handlers.rs`): Processes various system actions
//!
//! ### Communication
//!
//! - **Message Handling** (`message.rs`): Protobuf message serialization/deserialization
//! - **Protocol Buffers** (`protos/`): Message definitions for network communication
//! - **Consensus Messages** (`protos/messages/v1/consensus.proto`): Consensus-specific message types
//!
//! ## Steward Epoch Flow
//!
//! The system operates in epochs managed by a steward:
//!
//! 1. **Working State**: Normal operation, all users can send messages
//! 2. **Waiting State**: Steward epoch active, collecting proposals
//! 3. **Voting State**: Consensus voting on collected proposals
//!
//! ### State Transitions
//!
//! ```text
//! Working --start_steward_epoch()--> Waiting (if proposals exist)
//! Working --start_steward_epoch()--> Working (if no proposals)
//! Waiting --start_voting()---------> Voting
//! Voting --complete_voting(true)--> Waiting (vote passed)
//! Voting --complete_voting(false)-> Working (vote failed)
//! Waiting --apply_proposals_and_complete()--> Working
//! ```
//!
//! ## Message Flow
//!
//! ### Regular Messages
//! ```text
//! User --> Group --> MLS Group --> Other Users
//! ```
//!
//! ### Steward Messages
//! ```text
//! Steward (with proposals) --> Group --> MLS Group --> Other Users
//! ```
//!
//! ### Batch Proposals
//! ```text
//! Group --> Create MLS Proposals --> Commit --> Batch Message --> Users
//! Users --> Parse Batch --> Apply Proposals --> Update MLS Group
//! ```
//!
//! ## Testing
//!
//! The system includes comprehensive tests:
//!
//! - State machine transitions
//! - Message handling
//!
//! Run tests with:
//! ```bash
//! cargo test
//! ```
//!
//! ## Dependencies
//!
//! - **MLS**: Message Layer Security for group key management
//! - **Tokio**: Async runtime for concurrent operations
//! - **Kameo**: Actor system for distributed operations
//! - **Prost**: Protocol buffer serialization
//! - **OpenMLS**: MLS implementation
//! - **Waku**: Decentralized messaging protocol
//! - **Alloy**: Ethereum wallet and signing

use ecies::{decrypt, encrypt};
use libsecp256k1::{sign, verify, Message, PublicKey, SecretKey, Signature as libSignature};
use rand::thread_rng;
use secp256k1::hashes::{sha256, Hash};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::Sender;
use waku_bindings::{WakuContentTopic, WakuMessage};

use ds::waku_actor::WakuMessageToSend;
use error::{GroupError, MessageError};

pub mod action_handlers;
// pub mod consensus;
pub mod error;
pub mod group;
pub mod message;
pub mod state_machine;
pub mod steward;
pub mod user;
pub mod user_actor;
pub mod user_app_instance;
pub mod ws_actor;

pub mod protos {
    pub mod messages {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/de_mls.messages.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/consensus.v1.rs"));
        }
    }
}

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
    use super::{decrypt_message, encrypt_message, generate_keypair, sign_message, verify_message};

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
