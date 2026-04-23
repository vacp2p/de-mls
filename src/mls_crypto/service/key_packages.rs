use openmls_rust_crypto::MemoryStorage;
use openmls_traits::OpenMlsProvider;

use crate::mls_crypto::{
    CIPHERSUITE, DeMlsStorage, IdentityError, KeyPackageBytes, MlsError, MlsService, Result,
    StorageError,
};

impl<S> MlsService<S>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // ══════════════════════════════════════════════════════════
    // Key Packages
    // ══════════════════════════════════════════════════════════

    /// Generate a key package for joining a group.
    ///
    /// Key packages are single-use and should be regenerated after each join.
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes> {
        let guard = self
            .identity
            .read()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        let identity = guard
            .as_ref()
            .ok_or(MlsError::Identity(IdentityError::IdentityNotFound))?;

        let provider = self.make_provider();

        let kp_bundle = openmls::key_packages::KeyPackage::builder().build(
            CIPHERSUITE,
            &provider,
            &identity.signer,
            identity.credential.clone(),
        )?;

        let kp = kp_bundle.key_package();
        let hash_ref = kp.hash_ref(provider.crypto())?.as_slice().to_vec();
        let bytes = serde_json::to_vec(kp).map_err(IdentityError::InvalidJson)?;

        self.storage.store_key_package_ref(&hash_ref)?;

        Ok(KeyPackageBytes::new(
            bytes,
            identity.wallet.as_slice().to_vec(),
        ))
    }
}
