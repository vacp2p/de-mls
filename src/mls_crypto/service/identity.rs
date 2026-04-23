use alloy::primitives::Address;
use openmls::prelude::{BasicCredential, CredentialWithKey};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::MemoryStorage;

use crate::mls_crypto::{
    CIPHERSUITE, DeMlsStorage, IdentityError, MlsError, MlsService, Result, StorageError,
    identity::IdentityData,
};

impl<S> MlsService<S>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // ══════════════════════════════════════════════════════════
    // Identity
    // ══════════════════════════════════════════════════════════

    /// Initialize identity from wallet address.
    ///
    /// Creates MLS credentials and signing keys from the wallet address.
    /// Call this once before using any other methods.
    pub fn init(&self, wallet: Address) -> Result<()> {
        {
            let guard = self
                .identity
                .read()
                .map_err(|e| StorageError::Lock(e.to_string()))?;
            if guard.is_some() {
                return Err(MlsError::Identity(IdentityError::AlreadyInitialized));
            }
        }

        let credential = BasicCredential::new(wallet.as_slice().to_vec());
        let signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm())?;

        // Store signer in OpenMLS storage
        signer.store(self.storage.mls_storage())?;

        let data = IdentityData {
            wallet,
            credential: CredentialWithKey {
                credential: credential.into(),
                signature_key: signer.to_public_vec().into(),
            },
            signer,
        };

        let mut guard = self
            .identity
            .write()
            .map_err(|e| StorageError::Lock(e.to_string()))?;
        *guard = Some(data);
        Ok(())
    }

    /// Get the wallet address as a checksummed hex string ("0x...").
    pub fn wallet_hex(&self) -> String {
        self.identity
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|id| id.wallet.to_checksum(None)))
            .unwrap_or_default()
    }

    /// Get the wallet address as raw bytes.
    pub fn wallet_bytes(&self) -> Vec<u8> {
        self.identity
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|id| id.wallet.as_slice().to_vec()))
            .unwrap_or_default()
    }
}
