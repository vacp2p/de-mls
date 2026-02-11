/// Errors originating from the delivery service layer.
///
/// String payloads carry the underlying libwaku error message. These are
/// human-readable but not structured — callers should treat them as opaque
/// diagnostic text, not match on their content.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Waku publish message error: {0}")]
    WakuPublishMessageError(String),
    #[error("Waku node already initialized: {0}")]
    WakuNodeAlreadyInitialized(String),
    #[error("Waku connect peer error: {0}")]
    WakuConnectPeerError(String),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
