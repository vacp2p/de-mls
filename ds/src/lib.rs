pub mod ds_waku;
pub mod waku_actor;

#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[error("Waku publish message error: {0}")]
    WakuPublishMessageError(String),
    #[error("Waku relay topics error: {0}")]
    WakuRelayTopicsError(String),
    #[error("Waku subscribe to group error: {0}")]
    WakuSubscribeToGroupError(String),
    #[error("Waku receive message error: {0}")]
    WakuReceiveMessageError(String),
    #[error("Waku node already initialized: {0}")]
    WakuNodeAlreadyInitialized(String),
    #[error("Waku subscribe to content filter error: {0}")]
    WakuSubscribeToContentFilterError(String),
    #[error("Waku add peer error: {0}")]
    WakuAddPeerError(String),
    #[error("Waku connect peer error: {0}")]
    WakuConnectPeerError(String),
    #[error("Waku get listen addresses error: {0}")]
    WakuGetListenAddressesError(String),

    #[error("Failed to parse multiaddr: {0}")]
    FailedToParseMultiaddr(String),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
