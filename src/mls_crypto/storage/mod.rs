//! Storage abstraction for DE-MLS persistence.
//!
//! This module provides the `DeMlsStorage` trait for pluggable storage backends.
//! Use `MemoryDeMlsStorage` for development/testing, or implement your own for persistence.

mod memory;

pub use memory::MemoryDeMlsStorage;

use crate::mls_crypto::error::StorageError;

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

    /// Store a key package hash reference as "ours".
    fn store_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), StorageError>;

    /// Check if a key package hash reference belongs to us.
    fn is_our_key_package(&self, hash_ref: &[u8]) -> Result<bool, StorageError>;

    /// Remove a key package reference (after it's used in a welcome).
    fn remove_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), StorageError>;

    // ─────────────────────────────────────────────────────────
    // OpenMLS Storage Delegation
    // ─────────────────────────────────────────────────────────

    /// Get the OpenMLS storage provider.
    ///
    /// OpenMLS calls this synchronously for all MLS state persistence.
    fn mls_storage(&self) -> &Self::MlsStorage;
}
