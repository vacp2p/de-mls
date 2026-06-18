//! Shared fixtures for de-mls integration tests.
//!
//! A minimal [`ConversationPluginsFactory`] over the OpenMLS reference provider
//! (`OpenMlsRustCrypto`), plus the credential/key-package helpers a test needs.
//! The library names no concrete provider; tests supply this one.
#![allow(dead_code)]

pub mod harness;
pub mod wallet;

use std::cell::RefCell;

use de_mls::defaults::{DefaultPeerScoring, DefaultStewardList, InMemoryPeerScoreStorage};
use de_mls::mls_crypto::{KeyPackageBytes, MlsError, OpenMlsService};
use de_mls::{
    ConversationPluginsFactory, DeterministicStewardList, PeerScoringService, ScoringConfig,
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

/// MLS service type for the test factory: the reference engine over
/// `OpenMlsRustCrypto`.
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

/// Reference plug-in factory over `OpenMlsRustCrypto`. Holds the member's
/// credential + signer; mints key packages and (per-conversation) the MLS
/// engine. Mirrors what an integrator wires up.
pub struct TestPluginsFactory {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    /// Provider stashed by [`Self::generate_key_package`] so the matching
    /// [`Self::welcome_mls`] can reuse it (it holds the KP's private keys).
    pending_provider: RefCell<Option<OpenMlsRustCrypto>>,
}

impl TestPluginsFactory {
    pub fn new(credential: CredentialWithKey, signer: SignatureKeyPair) -> Self {
        Self {
            credential,
            signer,
            pending_provider: RefCell::new(None),
        }
    }

    /// Mint a single-use key package in a fresh provider, stashing that
    /// provider so a later `welcome_mls` can join with the KP's private keys.
    pub fn generate_key_package(&self) -> KeyPackageBytes {
        let provider = OpenMlsRustCrypto::default();
        let member_id = self.credential.credential.serialized_content().to_vec();
        let bundle = KeyPackage::builder()
            .build(TEST_SUITE, &provider, &self.signer, self.credential.clone())
            .expect("key package");
        let bytes = bundle
            .key_package()
            .tls_serialize_detached()
            .expect("kp tls");
        *self.pending_provider.borrow_mut() = Some(provider);
        KeyPackageBytes::new(bytes, member_id)
    }
}

impl ConversationPluginsFactory for TestPluginsFactory {
    type Mls = TestMls;
    type Scoring = DefaultPeerScoring;
    type StewardList = DefaultStewardList;

    fn create_mls(
        &self,
        conversation_id: String,
        key_package: &[u8],
        signer: &impl Signer,
    ) -> Result<Self::Mls, MlsError> {
        OpenMlsService::new_as_creator(
            conversation_id,
            OpenMlsRustCrypto::default(),
            key_package,
            signer,
        )
    }

    fn welcome_mls(&self, welcome_bytes: &[u8]) -> Result<Option<Self::Mls>, MlsError> {
        // No stashed provider (we never minted a KP) → a fresh empty provider
        // holds no matching key package, so the join cleanly yields `None`.
        let provider = self
            .pending_provider
            .borrow_mut()
            .take()
            .unwrap_or_default();
        OpenMlsService::new_from_welcome(welcome_bytes, provider)
    }

    fn make_scoring(&self, config: &ScoringConfig) -> Self::Scoring {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            default_score_deltas(),
            config.clone(),
        )
    }

    fn make_steward_list(
        &self,
        conversation_id: &[u8],
        config: StewardListConfig,
    ) -> Self::StewardList {
        DeterministicStewardList::empty(conversation_id.to_vec(), config)
    }
}
