//! Gateway-side MLS defaults: the concrete plug-in helper the reference
//! integrator wires into [`User`](crate::user::User).
//!
//! de-mls names no concrete MLS backend — `Conversation` takes an
//! `OpenMlsProvider` by reference on every driving call and builds the MLS
//! service from a credential + ciphersuite + provider. This module supplies the
//! integrator half: the OpenMLS reference provider (`OpenMlsRustCrypto`),
//! credential generation from a member id, key-package minting, and the
//! concrete scoring / steward-list plug-in instances the `User` passes into
//! [`Conversation::create`](de_mls::Conversation::create) /
//! [`Conversation::join`](de_mls::Conversation::join).

use de_mls::{
    DeterministicStewardList, PeerScoringService, ScoringConfig, StewardListConfig,
    default_score_deltas,
    defaults::{DefaultPeerScoring, DefaultStewardList, InMemoryPeerScoreStorage},
    mls_crypto::{KeyPackageBytes, MlsError},
};
use openmls::{
    credentials::{BasicCredential, CredentialWithKey},
    key_packages::KeyPackage,
    prelude::{Ciphersuite, tls_codec::Serialize as _},
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

/// Ciphersuite the gateway pins for every conversation it creates or joins.
pub const GATEWAY_SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// OpenMLS provider the gateway runs: the reference RustCrypto backend.
pub type GatewayProvider = OpenMlsRustCrypto;

/// Build a fresh MLS credential + signing keypair for `member_id`. The
/// member-id bytes become the credential's serialized content, so the MLS
/// leaf and the protocol's "who is this?" checks agree.
pub fn build_credential(
    member_id: &[u8],
) -> Result<(CredentialWithKey, SignatureKeyPair), MlsError> {
    let signer = SignatureKeyPair::new(GATEWAY_SUITE.signature_algorithm())?;
    let credential = CredentialWithKey {
        credential: BasicCredential::new(member_id.to_vec()).into(),
        signature_key: signer.to_public_vec().into(),
    };
    Ok((credential, signer))
}

/// Reference plug-in helper over `OpenMlsRustCrypto`. Holds the member's
/// credential + signer, owns the single OpenMLS provider every conversation
/// borrows, mints key packages, and builds per-conversation plug-in instances
/// the `User` feeds into the de-mls constructors. One per `User`.
pub struct DefaultConversationPluginsFactory {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    /// The one OpenMLS provider for this `User`, borrowed on every driving
    /// call. The creator seeds its group into it; a joiner mints its key
    /// package into it, and the matching welcome opens against the same
    /// provider — the key package's private keys are already there.
    provider: OpenMlsRustCrypto,
}

impl DefaultConversationPluginsFactory {
    pub fn new(credential: CredentialWithKey, signer: SignatureKeyPair) -> Self {
        Self {
            credential,
            signer,
            provider: OpenMlsRustCrypto::default(),
        }
    }

    /// The member's MLS credential, handed to [`Conversation::create`]
    /// (de_mls::Conversation::create) on the creator path.
    pub fn credential(&self) -> CredentialWithKey {
        self.credential.clone()
    }

    /// The User's OpenMLS provider, borrowed into every `Conversation` call.
    pub fn provider(&self) -> &OpenMlsRustCrypto {
        &self.provider
    }

    /// Mint a single-use key package into the User's provider so a later join
    /// can open the welcome with the key package's private keys.
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        let member_id = self.credential.credential.serialized_content().to_vec();
        let bundle = KeyPackage::builder().build(
            GATEWAY_SUITE,
            &self.provider,
            &self.signer,
            self.credential.clone(),
        )?;
        let bytes = bundle
            .key_package()
            .tls_serialize_detached()
            .map_err(MlsError::KeyPackageTls)?;
        Ok(KeyPackageBytes::new(bytes, member_id))
    }

    /// Build a fresh peer-scoring plug-in.
    pub fn make_scoring(&self, config: &ScoringConfig) -> DefaultPeerScoring {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            default_score_deltas(),
            config.clone(),
        )
    }

    /// Build a fresh (empty) steward-list plug-in. The library stamps the
    /// conversation-id sort salt when it builds the conversation, so the
    /// gateway supplies none.
    pub fn make_steward(&self, config: StewardListConfig) -> DefaultStewardList {
        DeterministicStewardList::empty(config)
    }
}
