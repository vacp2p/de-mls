//! [`DefaultKeyPackageProvider`] — reference impl of
//! [`crate::core::KeyPackageProvider`].
//!
//! Wraps an `Arc<MlsCredentials>` + `Arc<MemoryDeMlsStorage>` (the same
//! handles used by [`super::DefaultConversationPluginsFactory`] for MLS
//! service construction). Generates a fresh single-use key package on each
//! `generate()` call.

use std::sync::Arc;

use crate::{
    core::KeyPackageProvider,
    mls_crypto::{KeyPackageBytes, MemoryDeMlsStorage, MlsCredentials, MlsError, OpenMlsService},
};

/// Default reference implementation of [`KeyPackageProvider`] backed by
/// in-memory MLS storage + a User-level [`MlsCredentials`].
pub struct DefaultKeyPackageProvider {
    pub(crate) storage: Arc<MemoryDeMlsStorage>,
    pub(crate) credentials: Arc<MlsCredentials>,
}

impl KeyPackageProvider for DefaultKeyPackageProvider {
    fn generate(&self) -> Result<KeyPackageBytes, MlsError> {
        OpenMlsService::<Arc<MemoryDeMlsStorage>>::generate_key_package(
            &self.storage,
            &self.credentials,
        )
    }
}
