use std::collections::HashMap;

use openmls::credentials::CredentialWithKey;
use openmls::key_packages::*;
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::types::Ciphersuite;

use crate::openmls_provider::CryptoProvider;

pub struct Identity {
    pub(crate) kp: HashMap<Vec<u8>, KeyPackage>,
    pub(crate) credential_with_key: CredentialWithKey,
    pub(crate) signer: SignatureKeyPair,
}

pub trait IdentityService {
    fn get_signature_key_pair(ciphersuite: Ciphersuite) -> SignatureKeyPair {
        SignatureKeyPair::new(ciphersuite.signature_algorithm()).unwrap()
    }
}

impl Identity {
    pub(crate) fn new(ciphersuite: Ciphersuite, crypto: &CryptoProvider, username: &[u8]) -> Self {
        let credential = Credential::new(username.to_vec(), CredentialType::Basic).unwrap();
        let signature_keys = SignatureKeyPair::new(ciphersuite.signature_algorithm()).unwrap();
        let credential_with_key = CredentialWithKey {
            credential,
            signature_key: signature_keys.to_public_vec().into(),
        };
        signature_keys.store(crypto.key_store()).unwrap();

        let key_package = KeyPackage::builder()
            .build(
                CryptoConfig {
                    ciphersuite,
                    version: ProtocolVersion::default(),
                },
                crypto,
                &signature_keys,
                credential_with_key.clone(),
            )
            .unwrap();

        Self {
            kp: HashMap::from([(
                key_package
                    .hash_ref(crypto.crypto())
                    .unwrap()
                    .as_slice()
                    .to_vec(),
                key_package.clone(),
            )]),
            credential_with_key,
            signer: signature_keys,
        }
    }

    /// Create an additional key package using the credential_with_key/signer bound to this identity
    pub fn add_key_package(
        &mut self,
        ciphersuite: Ciphersuite,
        crypto: &CryptoProvider,
    ) -> KeyPackage {
        let key_package = KeyPackage::builder()
            .build(
                CryptoConfig::with_default_version(ciphersuite),
                crypto,
                &self.signer,
                self.credential_with_key.clone(),
            )
            .unwrap();

        self.kp.insert(
            key_package
                .hash_ref(crypto.crypto())
                .unwrap()
                .as_slice()
                .to_vec(),
            key_package.clone(),
        );
        key_package.clone()
    }

    /// Get the plain identity as byte vector.
    pub fn identity(&self) -> Vec<u8> {
        self.credential_with_key.credential.identity().to_vec()
    }

    /// Get the plain identity as byte vector.
    pub fn identity_as_string(&self) -> String {
        std::str::from_utf8(self.credential_with_key.credential.identity())
            .unwrap()
            .to_string()
    }
}
