use alloy::{hex::FromHexError, primitives::SignatureError};
use fred::error::RedisError;

pub mod chat_client;
pub mod chat_server;
pub mod ds;
pub mod ds_waku;

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

    #[error("Unable to create waku node: {0}")]
    WakuCreateNodeError(String),
    #[error("Invalid waku message: {0}")]
    WakuInvalidMessage(String),
    #[error("Waku relay error: {0}")]
    WakuRelayError(String),
    #[error("Waku stop node error: {0}")]
    WakuStopNodeError(String),
    #[error("Waku publish message error: {0}")]
    WakuPublishMessageError(String),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
