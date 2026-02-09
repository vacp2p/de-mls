//! In-memory storage implementation.

use std::collections::HashSet;
use std::sync::RwLock;

use openmls_rust_crypto::MemoryStorage;

use super::DeMlsStorage;
use crate::mls_crypto::error::StorageError;

/// In-memory storage for development and testing.
///
/// All data is lost on restart. Use a persistent storage implementation
/// (e.g., SQLite) for production use cases.
#[derive(Default)]
pub struct MemoryDeMlsStorage {
    key_package_refs: RwLock<HashSet<Vec<u8>>>,
    mls: MemoryStorage,
}

impl MemoryDeMlsStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl DeMlsStorage for MemoryDeMlsStorage {
    type MlsStorage = MemoryStorage;
    type StorageError = openmls_rust_crypto::MemoryStorageError;

    fn store_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), StorageError> {
        self.key_package_refs
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .insert(hash_ref.to_vec());
        Ok(())
    }

    fn is_our_key_package(&self, hash_ref: &[u8]) -> Result<bool, StorageError> {
        Ok(self
            .key_package_refs
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .contains(hash_ref))
    }

    fn remove_key_package_ref(&self, hash_ref: &[u8]) -> Result<(), StorageError> {
        self.key_package_refs
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?
            .remove(hash_ref);
        Ok(())
    }

    fn mls_storage(&self) -> &Self::MlsStorage {
        &self.mls
    }
}
