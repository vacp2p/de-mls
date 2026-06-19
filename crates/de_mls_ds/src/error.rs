#[cfg(feature = "waku")]
use crate::waku::wrapper::WakuFfiError;

/// Errors originating from the delivery service layer.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryServiceError {
    #[cfg(feature = "waku")]
    #[error("Waku publish failed: {0}")]
    WakuPublish(#[source] WakuFfiError),

    #[cfg(feature = "waku")]
    #[error("Waku node startup failed: {0}")]
    WakuStartup(#[source] WakuFfiError),

    #[cfg(feature = "waku")]
    #[error("Waku connect peer failed: {0}")]
    WakuConnectPeer(#[source] WakuFfiError),

    #[error("Failed to spawn delivery-service thread: {0}")]
    ThreadSpawn(#[source] std::io::Error),

    #[cfg(feature = "waku")]
    #[error("Waku node channel closed")]
    WakuChannelClosed,

    #[error("Lock poisoned: {0}")]
    LockPoisoned(&'static str),
}
