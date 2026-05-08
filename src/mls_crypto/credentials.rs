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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal test-only [`Identity`] impl — pure bytes, no wallet
    /// machinery. Verifies `from_identity` doesn't depend on
    /// `WalletIdentity` specifically.
    struct TestIdentity {
        bytes: Vec<u8>,
        display: String,
    }

    impl Identity for TestIdentity {
        fn identity_bytes(&self) -> &[u8] {
            &self.bytes
        }
        fn identity_display(&self) -> &str {
            &self.display
        }
    }

    fn test_identity(bytes: &[u8]) -> TestIdentity {
        TestIdentity {
            bytes: bytes.to_vec(),
            display: format!("test:{}", bytes.len()),
        }
    }

    /// Load-bearing invariant: the credential's serialized content
    /// equals `identity.identity_bytes()`. Membership comparisons,
    /// score keys, and "is this commit from me" checks all rely on
    /// this round-trip — if it ever drifts, downstream comparisons
    /// silently misbehave.
    #[test]
    fn credential_serialized_content_equals_identity_bytes() {
        let identity = test_identity(b"alice-bytes");
        let creds = MlsCredentials::from_identity(&identity).unwrap();
        assert_eq!(
            creds.credential().credential.serialized_content(),
            b"alice-bytes",
        );
    }

    /// Each `from_identity` call must mint a fresh signing keypair —
    /// no static keys leaking through `Default` impls or shared
    /// crypto state.
    #[test]
    fn fresh_keypair_per_call() {
        let identity = test_identity(b"alice");
        let a = MlsCredentials::from_identity(&identity).unwrap();
        let b = MlsCredentials::from_identity(&identity).unwrap();
        assert_ne!(a.signer().to_public_vec(), b.signer().to_public_vec());
    }

    /// The credential's public `signature_key` must match the
    /// signer's public part — otherwise commits we sign won't
    /// verify under the credential we publish.
    #[test]
    fn credential_signature_key_matches_signer_public() {
        let identity = test_identity(b"alice");
        let creds = MlsCredentials::from_identity(&identity).unwrap();
        assert_eq!(
            creds.credential().signature_key.as_slice(),
            creds.signer().public(),
        );
    }
}
