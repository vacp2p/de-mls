use alloy::{hex::FromHexError, primitives::SignatureError};
use fred::error::RedisError;

pub mod chat_client;
pub mod chat_server;
pub mod ds;

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Validation failed: {0}")]
    ValidationError(String),

    #[error("Redis operation failed: {0}")]
    RedisError(#[from] RedisError),
    #[error("Failed to send message to channel: {0}")]
    SenderError(String),
    #[error("WebSocket handshake failed.")]
    HandshakeError(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("Serialization error: {0}")]
    TlsError(#[from] tls_codec::Error),
    #[error("JSON processing error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Failed to bind to the address.")]
    BindError(#[from] std::io::Error),

    #[error("Failed to parse address: {0}")]
    AlloyFromHexError(#[from] FromHexError),
    #[error("Failed to recover signature: {0}")]
    AlloySignatureError(#[from] SignatureError),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
