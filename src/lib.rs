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
//! The system operates in epochs managed by a steward with robust state management:
//!
//! 1. **Working State**: Normal operation, all users can send any message freely
//! 2. **Waiting State**: Steward epoch active, only steward can send BATCH_PROPOSALS_MESSAGE
//! 3. **Voting State**: Consensus voting, restricted message types (VOTE/USER_VOTE for all, VOTING_PROPOSAL/PROPOSAL for steward only)
//!
//! ### Complete State Transitions
//!
//! ```text
//! Working --start_steward_epoch()--> Waiting (if proposals exist)
//! Working --start_steward_epoch()--> Working (if no proposals - no state change)
//! Waiting --start_voting()---------> Voting
//! Waiting --no_proposals_found()---> Working (edge case: proposals disappear during voting)
//! Voting --complete_voting(YES)----> Waiting --apply_proposals()--> Working
//! Voting --complete_voting(NO)-----> Working
//! ```
//!
//! ### Steward State Guarantees
//!
//! - **Always returns to Working**: Steward transitions back to Working state after every epoch
//! - **No proposals handling**: If no proposals exist, steward stays in Working state
//! - **Edge case coverage**: All scenarios including proposal disappearance are handled
//! - **Robust error handling**: Invalid state transitions are prevented and logged
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

use alloy::primitives::{Address, Signature};
use ecies::{decrypt, encrypt};
use libsecp256k1::{sign, verify, Message, PublicKey, SecretKey, Signature as libSignature};
use rand::thread_rng;
use secp256k1::hashes::{sha256, Hash};

use error::{GroupError, MessageError};

pub mod bootstrap;
pub use bootstrap::{bootstrap_core, bootstrap_core_from_env, Bootstrap, BootstrapConfig};

pub mod consensus;
pub mod error;
pub mod group;
pub mod group_registry;
pub mod message;
pub mod state_machine;
pub mod steward;
pub mod user;
pub mod user_actor;
pub mod user_app_instance;

pub mod protos {
    pub mod consensus {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/consensus.v1.rs"));
        }
    }

    pub mod de_mls {
        pub mod messages {
            pub mod v1 {
                include!(concat!(env!("OUT_DIR"), "/de_mls.messages.v1.rs"));
            }
        }
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
    const COMPRESSED_PUBLIC_KEY_SIZE: usize = 33;

    let digest = sha256::Hash::hash(message);
    let msg = Message::parse(&digest.to_byte_array());
    let signature = libSignature::parse_der(signature)?;

    let mut pub_key_bytes: [u8; COMPRESSED_PUBLIC_KEY_SIZE] = [0; COMPRESSED_PUBLIC_KEY_SIZE];
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

pub trait LocalSigner {
    fn local_sign_message(
        &self,
        message: &[u8],
    ) -> impl std::future::Future<Output = Result<Vec<u8>, anyhow::Error>> + Send;

    fn address(&self) -> Address;
    fn address_string(&self) -> String;
    fn address_bytes(&self) -> Vec<u8>;
}

pub fn verify_vote_hash(
    signature: &[u8],
    public_key: &[u8],
    message: &[u8],
) -> Result<bool, MessageError> {
    let signature_bytes: [u8; 65] =
        signature
            .try_into()
            .map_err(|_| MessageError::MismatchedLength {
                expect: 65,
                actual: signature.len(),
            })?;
    let signature = Signature::from_raw_array(&signature_bytes)?;
    let address = signature.recover_address_from_msg(message)?;
    let address_bytes = address.as_slice().to_vec();
    Ok(address_bytes == public_key)
}

#[cfg(test)]
mod tests {
    use alloy::signers::local::PrivateKeySigner;

    use crate::{verify_vote_hash, LocalSigner};

    use super::{decrypt_message, encrypt_message, generate_keypair, sign_message, verify_message};

    #[test]
    fn test_verify_message() {
        let message = b"Hello, world!";
        let (public_key, secret_key) = generate_keypair();
        let signature = sign_message(message, &secret_key);
        let verified = verify_message(message, &signature, &public_key.serialize_compressed())
            .expect("Failed to verify message");
        assert!(verified);
    }

    #[test]
    fn test_encrypt_decrypt_message() {
        let message = b"Hello, world!";
        let (public_key, secret_key) = generate_keypair();
        let encrypted = encrypt_message(message, &public_key.serialize_compressed())
            .expect("Failed to encrypt message");
        let decrypted = decrypt_message(&encrypted, secret_key).expect("Failed to decrypt message");
        assert_eq!(message, decrypted.as_slice());
    }

    #[tokio::test]
    async fn test_local_signer() {
        let signer = PrivateKeySigner::random();
        let message = b"Hello, world!";
        let signature = signer
            .local_sign_message(message)
            .await
            .expect("Failed to sign message");

        let verified = verify_vote_hash(&signature, &signer.address_bytes(), message)
            .expect("Failed to verify vote hash");
        assert!(verified);
    }
}
