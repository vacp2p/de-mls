pub mod ds_waku;

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Invalid waku message: {0}")]
    WakuInvalidMessage(String),
    #[error("Waku publish message error: {0}")]
    WakuPublishMessageError(String),
    #[error("Waku relay topics error: {0}")]
    WakuRelayTopicsError(String),
    #[error("Waku invalid content topic: {0}")]
    WakuInvalidContentTopic(String),
    #[error("Waku node config error: {0}")]
    WakuNodeConfigError(String),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
