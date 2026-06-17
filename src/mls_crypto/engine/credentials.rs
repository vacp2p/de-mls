//! MLS-specific credential bundle: signing keypair + credential.
//!
//! Built once per user from an [`crate::member_id::MemberId`] at User init
//! and shared across every per-conversation.

use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls_basic_credential::SignatureKeyPair;

use crate::{
    member_id::MemberId,
    mls_crypto::{MlsError, engine::CIPHERSUITE},
};

/// MLS credential + signing keypair for one user, shared across all
/// conversations they belong to.
#[derive(Debug)]
pub struct MlsCredentials {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
}

impl MlsCredentials {
    /// Build credentials from an [`MemberId`].
    pub fn from_member_id<I: MemberId + ?Sized>(member_id: &I) -> Result<Self, MlsError> {
        let credential = BasicCredential::new(member_id.member_id_bytes().to_vec());
        let signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm())?;
        Ok(Self {
            credential: CredentialWithKey {
                credential: credential.into(),
                signature_key: signer.to_public_vec().into(),
            },
            signer,
        })
    }

    pub fn credential(&self) -> &CredentialWithKey {
        &self.credential
    }

    pub fn signer(&self) -> &SignatureKeyPair {
        &self.signer
    }
}
