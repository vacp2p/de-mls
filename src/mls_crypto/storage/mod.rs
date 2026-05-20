//! Storage abstraction for DE-MLS persistence.
//!

use std::sync::Arc;

use crate::mls_crypto::MlsError;

/// Storage backend for DE-MLS.
///
/// Implementations must provide:
/// - Key package reference tracking (to detect welcomes for us)
/// - OpenMLS storage delegation (groups, key packages, etc.)
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync`. Internal synchronization
/// (locks, channels) is the implementation's responsibility.
pub trait DeMlsStorage: Send + Sync + 'static {
    /// The OpenMLS storage provider type.
    /// VERSION is the OpenMLS storage version (currently 1).
    type MlsStorage: openmls_traits::storage::StorageProvider<1, Error = Self::StorageError>;

    /// Storage error type (must be compatible with OpenMLS).
    type StorageError: std::error::Error + Send + Sync + 'static;

    // ─────────────────────────────────────────────────────────
    // Key Package Tracking
    // ─────────────────────────────────────────────────────────

    fn store_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), MlsError>;
    fn is_our_key_package(&self, hash_ref: &[u8]) -> Result<bool, MlsError>;
    fn remove_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), MlsError>;

    fn mls_storage(&self) -> &Self::MlsStorage;
}

/// Sharing impl: every `Arc<S>` over a [`DeMlsStorage`] is itself a
/// [`DeMlsStorage`]. Lets one storage backend back many MLS services
/// (one per group) so key-package refs and OpenMLS state remain
/// reachable from whichever service receives a welcome.
impl<S: DeMlsStorage + ?Sized> DeMlsStorage for Arc<S> {
    type MlsStorage = S::MlsStorage;
    type StorageError = S::StorageError;

    fn store_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), MlsError> {
        (**self).store_key_package_ref(hash_ref)
    }

    fn is_our_key_package(&self, hash_ref: &[u8]) -> Result<bool, MlsError> {
        (**self).is_our_key_package(hash_ref)
    }

    fn remove_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), MlsError> {
        (**self).remove_key_package_ref(hash_ref)
    }

    fn mls_storage(&self) -> &Self::MlsStorage {
        (**self).mls_storage()
    }
}
