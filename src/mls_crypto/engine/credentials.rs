//! MLS-specific credential bundle: signing keypair + credential + ciphersuite.
//!
//! Built once per user from an [`crate::member_id::MemberId`] and shared across
//! every per-conversation service. The signing keypair is ciphersuite-bound, so
//! the suite travels with it: [`OpenMlsService`](super::OpenMlsService) reads
//! [`ciphersuite`](MlsCredentials::ciphersuite) when building key packages and
//! the group rather than pinning a suite of its own.

use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls::prelude::Ciphersuite;
use openmls_basic_credential::SignatureKeyPair;

use crate::{
    member_id::MemberId,
    mls_crypto::{MlsError, engine::CIPHERSUITE},
};

/// MLS credential + signing keypair for one user, bound to one ciphersuite.
#[derive(Debug)]
pub struct MlsCredentials {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    ciphersuite: Ciphersuite,
}

impl MlsCredentials {
    /// Generate credentials for `member_id` under the default [`CIPHERSUITE`].
    pub fn from_member_id<I: MemberId + ?Sized>(member_id: &I) -> Result<Self, MlsError> {
        Self::generate(member_id, CIPHERSUITE)
    }

    /// Generate credentials for `member_id` under `ciphersuite`.
    pub fn generate<I: MemberId + ?Sized>(
        member_id: &I,
        ciphersuite: Ciphersuite,
    ) -> Result<Self, MlsError> {
        let credential = BasicCredential::new(member_id.member_id_bytes().to_vec());
        let signer = SignatureKeyPair::new(ciphersuite.signature_algorithm())?;
        Ok(Self {
            credential: CredentialWithKey {
                credential: credential.into(),
                signature_key: signer.to_public_vec().into(),
            },
            signer,
            ciphersuite,
        })
    }

    pub fn credential(&self) -> &CredentialWithKey {
        &self.credential
    }

    pub fn signer(&self) -> &SignatureKeyPair {
        &self.signer
    }

    /// The ciphersuite this credential's keypair was generated for.
    pub fn ciphersuite(&self) -> Ciphersuite {
        self.ciphersuite
    }
}
