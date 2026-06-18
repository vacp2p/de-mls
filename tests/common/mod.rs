//! Shared fixtures for de-mls integration tests.
//!
//! A minimal types-only [`ConversationPlugins`] bundle over the OpenMLS
//! reference provider (`OpenMlsRustCrypto`), plus free helpers that build the
//! per-conversation plug-in instances — exactly the inline wiring an integrator
//! does. The library names no concrete provider; tests supply this one.
#![allow(dead_code)]

pub mod harness;
pub mod wallet;

use de_mls::defaults::{DefaultPeerScoring, DefaultStewardList, InMemoryPeerScoreStorage};
use de_mls::mls_crypto::{KeyPackageBytes, MlsError, OpenMlsService};
use de_mls::{
    ConversationPlugins, DeterministicStewardList, PeerScoringService, ScoringConfig,
    StewardListConfig, default_score_deltas,
};
use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls::key_packages::KeyPackage;
use openmls::prelude::Ciphersuite;
use openmls::prelude::tls_codec::Serialize as _;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::signatures::Signer;

/// Ciphersuite the test fixtures pin.
pub const TEST_SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// MLS service type the tests run: the reference engine over `OpenMlsRustCrypto`.
pub type TestMls = OpenMlsService<OpenMlsRustCrypto>;

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

/// Types-only plug-in bundle over `OpenMlsRustCrypto`. The instances are built
/// by the free functions below — there is no factory object holding state.
pub struct TestPlugins;

impl ConversationPlugins for TestPlugins {
    type Mls = TestMls;
    type Scoring = DefaultPeerScoring;
    type StewardList = DefaultStewardList;
}

/// Mint a single-use key package into a fresh provider and hand both back. The
/// caller (the integrator) holds the returned provider until its welcome
/// arrives — it carries the KP's private keys needed to join.
pub fn mint_key_package(
    credential: &CredentialWithKey,
    signer: &SignatureKeyPair,
) -> (KeyPackageBytes, OpenMlsRustCrypto) {
    let provider = OpenMlsRustCrypto::default();
    let member_id = credential.credential.serialized_content().to_vec();
    let bundle = KeyPackage::builder()
        .build(TEST_SUITE, &provider, signer, credential.clone())
        .expect("key package");
    let bytes = bundle
        .key_package()
        .tls_serialize_detached()
        .expect("kp tls");
    (KeyPackageBytes::new(bytes, member_id), provider)
}

/// Build an MLS service seeding a brand-new conversation we create.
pub fn creator_mls(
    conversation_id: String,
    credential: CredentialWithKey,
    signer: &impl Signer,
) -> Result<TestMls, MlsError> {
    OpenMlsService::new_as_creator(
        conversation_id,
        OpenMlsRustCrypto::default(),
        credential,
        TEST_SUITE,
        signer,
    )
}

/// Try to open an MLS service from a serialized welcome using `provider` (which
/// must hold the joiner's key-package private keys). `Ok(None)` when the welcome
/// isn't for us.
pub fn open_welcome(
    welcome_bytes: &[u8],
    provider: OpenMlsRustCrypto,
) -> Result<Option<TestMls>, MlsError> {
    OpenMlsService::new_from_welcome(welcome_bytes, provider)
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
