/// Errors originating from the delivery service layer.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[cfg(feature = "waku")]
    #[error("Waku publish failed: {0}")]
    WakuPublish(#[source] crate::ds::waku::wrapper::WakuFfiError),

    #[cfg(feature = "waku")]
    #[error("Waku node startup failed: {0}")]
    WakuStartup(#[source] crate::ds::waku::wrapper::WakuFfiError),

    #[cfg(feature = "waku")]
    #[error("Waku connect peer failed: {0}")]
    WakuConnectPeer(#[source] crate::ds::waku::wrapper::WakuFfiError),

    #[error("An unknown error occurred: {0}")]
    Other(anyhow::Error),
}
