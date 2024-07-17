use hex;
use std::collections::HashMap;

use openmls::{credentials::CredentialWithKey, key_packages::*, prelude::*};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::MemoryKeyStoreError;
use openmls_traits::types::Ciphersuite;

use mls_crypto::openmls_provider::MlsCryptoProvider;

pub struct Identity {
    pub(crate) kp: HashMap<Vec<u8>, KeyPackage>,
    pub(crate) credential_with_key: CredentialWithKey,
    pub(crate) signer: SignatureKeyPair,
}

impl Identity {
    pub(crate) fn new(
        ciphersuite: Ciphersuite,
        crypto: &MlsCryptoProvider,
        user_wallet_address: &[u8],
    ) -> Result<Identity, IdentityError> {
        let credential = Credential::new(user_wallet_address.to_vec(), CredentialType::Basic)?;
        let signature_keys = SignatureKeyPair::new(ciphersuite.signature_algorithm())?;
        let credential_with_key = CredentialWithKey {
            credential,
            signature_key: signature_keys.to_public_vec().into(),
        };
        signature_keys.store(crypto.key_store())?;

        let key_package = KeyPackage::builder().build(
            CryptoConfig {
                ciphersuite,
                version: ProtocolVersion::default(),
            },
            crypto,
            &signature_keys,
            credential_with_key.clone(),
        )?;

        let kp = key_package.hash_ref(crypto.crypto())?;
        Ok(Identity {
            kp: HashMap::from([(kp.as_slice().to_vec(), key_package)]),
            credential_with_key,
            signer: signature_keys,
        })
    }

    /// Create an additional key package using the credential_with_key/signer bound to this identity
    pub fn add_key_package(
        &mut self,
        ciphersuite: Ciphersuite,
        crypto: &MlsCryptoProvider,
    ) -> Result<KeyPackage, IdentityError> {
        let key_package = KeyPackage::builder().build(
            CryptoConfig::with_default_version(ciphersuite),
            crypto,
            &self.signer,
            self.credential_with_key.clone(),
        )?;

        let kp = key_package.hash_ref(crypto.crypto())?;
        self.kp.insert(kp.as_slice().to_vec(), key_package.clone());
        Ok(key_package)
    }

    /// Get the plain identity as byte vector.
    pub fn identity(&self) -> Vec<u8> {
        self.credential_with_key.credential.identity().to_vec()
    }

    pub fn signature_pub_key(&self) -> Vec<u8> {
        self.signer.public().to_vec()
    }
}

impl ToString for Identity {
    fn to_string(&self) -> String {
        hex::encode(self.credential_with_key.credential.identity())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("Something wrong while creating new key package: {0}")]
    MlsKeyPackageNewError(#[from] KeyPackageNewError<MemoryKeyStoreError>),
    #[error(transparent)]
    MlsLibraryError(#[from] LibraryError),
    #[error("Something wrong with signature: {0}")]
    MlsCryptoError(#[from] CryptoError),
    #[error("Can't save signature key")]
    MlsKeyStoreError(#[from] MemoryKeyStoreError),
    #[error("Something wrong with credential: {0}")]
    MlsCredentialError(#[from] CredentialError),
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
