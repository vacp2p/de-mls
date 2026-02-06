#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Waku publish message error: {0}")]
    WakuPublishMessageError(String),
    #[error("Waku subscribe to pubsub topic error: {0}")]
    WakuSubscribeToPubsubTopicError(String),
    #[error("Waku node already initialized: {0}")]
    WakuNodeAlreadyInitialized(String),
    #[error("Waku connect peer error: {0}")]
    WakuConnectPeerError(String),
    #[error("Waku get listen addresses error: {0}")]
    WakuGetListenAddressesError(String),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
