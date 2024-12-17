use alloy::signers::local::LocalSignerError;
use ecies::{decrypt, encrypt};
use libsecp256k1::{sign, verify, Message, PublicKey, SecretKey, Signature as libSignature};
use openmls::{error::LibraryError, prelude::*};
use openmls_rust_crypto::MemoryKeyStoreError;
use rand::thread_rng;
use secp256k1::hashes::{sha256, Hash};
use std::{str::Utf8Error, string::FromUtf8Error};

use ds::DeliveryServiceError;
use sc_key_store::KeyStoreError;

pub mod conversation;
pub mod identity;
pub mod main_loop;
pub mod user;

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
pub enum UserError {
    #[error(transparent)]
    DeliveryServiceError(#[from] DeliveryServiceError),
    #[error(transparent)]
    KeyStoreError(#[from] KeyStoreError),
    #[error(transparent)]
    IdentityError(#[from] IdentityError),

    #[error("Group not found: {0}")]
    GroupNotFoundError(String),
    #[error("Group already exists: {0}")]
    GroupAlreadyExistsError(String),

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

    #[error("Failed to verify signature: {0}")]
    SignatureVerificationError(#[from] libsecp256k1::Error),
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
) -> Result<bool, UserError> {
    let digest = sha256::Hash::hash(message);
    let msg = Message::parse(&digest.to_byte_array());
    let signature = libSignature::parse_der(signature)?;
    let mut pub_key_bytes: [u8; 33] = [0; 33];
    pub_key_bytes[..].copy_from_slice(public_key);
    let public_key = PublicKey::parse_compressed(&pub_key_bytes)?;
    Ok(verify(&msg, &signature, &public_key))
}

pub fn encrypt_message(message: &[u8], public_key: &[u8]) -> Result<Vec<u8>, UserError> {
    let encrypted = encrypt(public_key, message)?;
    Ok(encrypted)
}

pub fn decrypt_message(message: &[u8], secret_key: SecretKey) -> Result<Vec<u8>, UserError> {
    let secret_key_serialized = secret_key.serialize();
    let decrypted = decrypt(&secret_key_serialized, message)?;
    Ok(decrypted)
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
