pub mod ds_waku;

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
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
    #[error("Waku already subscribed to the pubsub topic: {0}")]
    WakuAlreadySubscribed(String),
    #[error("Waku relay topics error: {0}")]
    WakuRelayTopicsError(String),
    #[error("Waku invalid content topic: {0}")]
    WakuInvalidContentTopic(String),

    #[error("Waku already received message: {0}")]
    WakuAlreadyReceived(String),

    #[error("Waku node config error: {0}")]
    WakuNodeConfigError(String),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
