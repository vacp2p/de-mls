//! Shared fixtures for de-mls integration tests.
//!
//! Helpers that build the per-conversation plug-in instances, credentials, and
//! key packages over the OpenMLS reference provider (`OpenMlsRustCrypto`) —
//! exactly the inline wiring an integrator does. The library builds the MLS
//! service itself from the provider + credential; tests supply those.
#![allow(dead_code)]

pub mod harness;
pub mod wallet;

use de_mls::defaults::{DefaultPeerScoring, DefaultStewardList, InMemoryPeerScoreStorage};
use de_mls::mls_crypto::KeyPackageBytes;
use de_mls::{
    DeterministicStewardList, PeerScoringService, ScoringConfig, StewardListConfig,
    default_score_deltas,
};
use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls::key_packages::KeyPackage;
use openmls::prelude::Ciphersuite;
use openmls::prelude::tls_codec::Serialize as _;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

/// Ciphersuite the test fixtures pin.
pub const TEST_SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// OpenMLS provider the tests run: the reference `OpenMlsRustCrypto`.
pub type TestProvider = OpenMlsRustCrypto;

/// Build a fresh credential + signer for `member_id` (the integrator-side
/// "credentials" the library no longer owns).
pub fn test_credential(member_id: &[u8]) -> (CredentialWithKey, SignatureKeyPair) {
    let signer = SignatureKeyPair::new(TEST_SUITE.signature_algorithm()).expect("signer");
    let credential = CredentialWithKey {
        credential: BasicCredential::new(member_id.to_vec()).into(),
        signature_key: signer.to_public_vec().into(),
    };
    (credential, signer)
}

/// Mint a single-use key package into `provider` — the integrator's one
/// reused provider, which thereby holds the KP's private keys needed to join
/// once the matching welcome arrives.
pub fn mint_key_package(
    provider: &TestProvider,
    credential: &CredentialWithKey,
    signer: &SignatureKeyPair,
) -> KeyPackageBytes {
    let member_id = credential.credential.serialized_content().to_vec();
    let bundle = KeyPackage::builder()
        .build(TEST_SUITE, provider, signer, credential.clone())
        .expect("key package");
    let bytes = bundle
        .key_package()
        .tls_serialize_detached()
        .expect("kp tls");
    KeyPackageBytes::new(bytes, member_id)
}

/// Build a fresh peer-scoring plug-in.
pub fn make_scoring(config: &ScoringConfig) -> DefaultPeerScoring {
    PeerScoringService::new(
        InMemoryPeerScoreStorage::new(),
        default_score_deltas(),
        config.clone(),
    )
}

/// Build a fresh (empty) steward-list plug-in.
pub fn make_steward(conversation_id: &[u8], config: StewardListConfig) -> DefaultStewardList {
    DeterministicStewardList::empty(conversation_id.to_vec(), config)
}
