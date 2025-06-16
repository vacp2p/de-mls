use alloy::{primitives::Address, signers::local::PrivateKeySigner};
use openmls::{credentials::CredentialWithKey, key_packages::KeyPackage, prelude::BasicCredential};
use openmls_basic_credential::SignatureKeyPair;
use openmls_traits::{types::Ciphersuite, OpenMlsProvider};
use std::{collections::HashMap, fmt::Display};

use crate::openmls_provider::{MlsProvider, CIPHERSUITE};
use crate::IdentityError;

pub struct Identity {
    pub(crate) kp: HashMap<Vec<u8>, KeyPackage>,
    pub(crate) credential_with_key: CredentialWithKey,
    pub(crate) signer: SignatureKeyPair,
}

impl Identity {
    pub fn new(
        ciphersuite: Ciphersuite,
        provider: &MlsProvider,
        user_wallet_address: &[u8],
    ) -> Result<Identity, IdentityError> {
        let credential = BasicCredential::new(user_wallet_address.to_vec());
        let signer = SignatureKeyPair::new(ciphersuite.signature_algorithm())?;
        let credential_with_key = CredentialWithKey {
            credential: credential.into(),
            signature_key: signer.to_public_vec().into(),
        };
        signer.store(provider.storage())?;

        let mut kps = HashMap::new();
        let key_package_bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            provider,
            &signer,
            credential_with_key.clone(),
        )?;
        let key_package = key_package_bundle.key_package();
        let kp = key_package.hash_ref(provider.crypto())?;
        kps.insert(kp.as_slice().to_vec(), key_package.clone());

        Ok(Identity {
            kp: kps,
            credential_with_key,
            signer,
        })
    }

    /// Create an additional key package using the credential_with_key/signer bound to this identity
    pub fn generate_key_package(
        &mut self,
        crypto: &MlsProvider,
    ) -> Result<KeyPackage, IdentityError> {
        let key_package_bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            crypto,
            &self.signer,
            self.credential_with_key.clone(),
        )?;
        let key_package = key_package_bundle.key_package();
        let kp = key_package.hash_ref(crypto.crypto())?;
        self.kp.insert(kp.as_slice().to_vec(), key_package.clone());
        Ok(key_package.clone())
    }

    /// Get the plain identity as byte vector.
    pub fn identity(&self) -> &[u8] {
        self.credential_with_key.credential.serialized_content()
    }

    pub fn identity_string(&self) -> String {
        address_string(self.credential_with_key.credential.serialized_content())
    }

    pub fn signature_pub_key(&self) -> Vec<u8> {
        self.signer.public().to_vec()
    }

    pub fn signer(&self) -> &SignatureKeyPair {
        &self.signer
    }

    pub fn credential_with_key(&self) -> CredentialWithKey {
        self.credential_with_key.clone()
    }

    pub fn signature_key(&self) -> Vec<u8> {
        self.credential_with_key.signature_key.as_slice().to_vec()
    }
}

impl Display for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            Address::from_slice(self.credential_with_key.credential.serialized_content())
        )
    }
}

pub fn address_string(identity: &[u8]) -> String {
    Address::from_slice(identity).to_string()
}

pub fn random_identity() -> Result<Identity, IdentityError> {
    let signer = PrivateKeySigner::random();
    let user_address = signer.address();

    let crypto = MlsProvider::default();
    let id = Identity::new(CIPHERSUITE, &crypto, user_address.as_slice())?;
    Ok(id)
}
