use alloy::primitives::Address;
use std::{collections::HashMap, fmt::Display};

use openmls::{credentials::CredentialWithKey, key_packages::*, prelude::*};
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::types::Ciphersuite;

use mls_crypto::openmls_provider::MlsCryptoProvider;

use crate::IdentityError;

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

        let mut kps = HashMap::new();
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
        kps.insert(kp.as_slice().to_vec(), key_package);

        Ok(Identity {
            kp: kps,
            credential_with_key,
            signer: signature_keys,
        })
    }

    /// Create an additional key package using the credential_with_key/signer bound to this identity
    pub fn generate_key_package(
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

    pub fn identity_string(&self) -> String {
        address_string(self.credential_with_key.credential.identity())
    }

    pub fn signature_pub_key(&self) -> Vec<u8> {
        self.signer.public().to_vec()
    }
}

impl Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            Address::from_slice(self.credential_with_key.credential.identity())
        )
    }
}

pub fn address_string(identity: &[u8]) -> String {
    Address::from_slice(identity).to_string()
}
