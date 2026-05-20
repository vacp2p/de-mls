//! MLS-specific credential bundle: signing keypair + credential.
//!
//! Built once per user from an [`crate::identity::Identity`] at User init
//! and shared across every per-conversation `MlsService` via
//! `Arc<MlsCredentials>`. The signing key is the long-lived MLS identity;
//! the credential's serialized content is the user's identity bytes —
//! the link back to whatever real-world identifier the
//! [`crate::identity::Identity`] represents.

use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls_basic_credential::SignatureKeyPair;

use crate::{
    identity::Identity,
    mls_crypto::{MlsError, service::CIPHERSUITE},
};

/// MLS credential + signing keypair for one user, shared across all
/// conversations they belong to.
#[derive(Debug)]
pub struct MlsCredentials {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
}

impl MlsCredentials {
    /// Build credentials from an [`Identity`]. Generates a fresh
    /// signing keypair (held only in this struct, not stored in MLS
    /// keystore until consumed by a service / KP build) and bundles it
    /// with a basic credential whose serialized content is
    /// `identity.identity_bytes()`.
    pub fn from_identity<I: Identity + ?Sized>(identity: &I) -> Result<Self, MlsError> {
        let credential = BasicCredential::new(identity.identity_bytes().to_vec());
        let signer = SignatureKeyPair::new(CIPHERSUITE.signature_algorithm())?;
        Ok(Self {
            credential: CredentialWithKey {
                credential: credential.into(),
                signature_key: signer.to_public_vec().into(),
            },
            signer,
        })
    }

    /// MLS credential bundle — public part of the identity, embedded in
    /// every signed MLS message we produce.
    pub fn credential(&self) -> &CredentialWithKey {
        &self.credential
    }

    /// MLS signing keypair — owns the private key used to sign MLS
    /// messages and proposals.
    pub fn signer(&self) -> &SignatureKeyPair {
        &self.signer
    }
}
