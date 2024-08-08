use alloy::{hex::FromHexError, primitives::SignatureError};

pub mod chat_client;
pub mod chat_server;
pub mod ds;

#[derive(thiserror::Error, Debug)]
pub enum ChatServiceError {
    #[error("Failed to bind to address")]
    BindError(#[from] std::io::Error),
    #[error("WebSocket handshake error")]
    HandshakeError(#[from] tokio_tungstenite::tungstenite::Error),

    #[error("Json error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Unable to parce the address: {0}")]
    AlloyFromHexError(#[from] FromHexError),

    #[error("Unable to recover the signature: {0}")]
    AlloySignatureError(#[from] SignatureError),

    #[error("Lock poisoned")]
    LockError,
    #[error("Send error")]
    SendError,
    #[error("Task error")]
    TaskError,
    #[error("Validation error")]
    ValidationError,
}
