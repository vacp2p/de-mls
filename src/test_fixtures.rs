//! Crate-internal test fixtures: a real creator-side MLS service builder over
//! `OpenMlsRustCrypto` plus minimal scoring / steward stubs, for tests that
//! construct a [`crate::Conversation`] without standing up real scoring or
//! steward backends.
//!
//! The stub scoring / steward methods are `unreachable!()` — tests should only
//! exercise the handful of paths the test specifically targets. If a test
//! reaches into an `unreachable!()` branch, that's a sign the test is touching
//! state it shouldn't be (and the panic location pinpoints the leak).

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;
use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls::prelude::Ciphersuite;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

use crate::{
    ConsensusPlugin, ConsensusServiceFor, StewardListConfig, StewardListService,
    defaults::DefaultConsensusPlugin, mls_crypto::OpenMlsService,
};

/// Build a `ConsensusServiceFor<DefaultConsensusPlugin>` paired with a
/// subscribed receiver for [`crate::Conversation::new`].
pub(crate) fn make_test_consensus_service() -> (
    ConsensusServiceFor<DefaultConsensusPlugin>,
    crate::defaults::SyncEventReceiver<String>,
) {
    use hashgraph_like_consensus::events::ConsensusEventBus;
    let service = ConsensusServiceFor::<DefaultConsensusPlugin>::new_with_components(
        DefaultConsensusPlugin::new_storage(),
        DefaultConsensusPlugin::new_event_bus(),
        EthereumConsensusSigner::new(PrivateKeySigner::random()),
        10,
    );
    let rx = service.event_bus().subscribe();
    (service, rx)
}

/// Ciphersuite the crate-internal fixtures pin.
pub(crate) const TEST_SUITE: Ciphersuite =
    Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Concrete OpenMLS provider the crate-internal fixtures run.
pub(crate) type TestProvider = OpenMlsRustCrypto;

/// MLS service type the crate-internal unit tests run: the reference engine.
pub(crate) type TestMls = OpenMlsService;

/// Build a real creator-side MLS service for `member_id` against a fresh
/// test-held provider, returning the service alongside the provider and the
/// signer that seeded it (both needed for any signing path the test
/// exercises). The guard tests this serves early-return before MLS advances,
/// but the service must still be a real, queryable group.
pub(crate) fn make_creator_mls(member_id: &[u8]) -> (TestMls, TestProvider, SignatureKeyPair) {
    let signer = SignatureKeyPair::new(TEST_SUITE.signature_algorithm()).expect("signer");
    let credential = CredentialWithKey {
        credential: BasicCredential::new(member_id.to_vec()).into(),
        signature_key: signer.to_public_vec().into(),
    };
    let provider = TestProvider::default();
    let mls = OpenMlsService::new_as_creator(
        "test-conversation".to_string(),
        &provider,
        credential,
        TEST_SUITE,
        &signer,
    )
    .expect("create creator mls");
    (mls, provider, signer)
}

/// Steward roster where the local member is NOT a steward (no list installed).
pub(crate) fn steward_service_member() -> StewardListService {
    StewardListService::empty(StewardListConfig::new(1, 5).unwrap())
}

/// Steward roster where `member_id` IS the sole steward.
pub(crate) fn steward_service_steward(member_id: &[u8]) -> StewardListService {
    let mut service = StewardListService::empty(StewardListConfig::new(1, 5).unwrap());
    service
        .install_list(0, &[member_id.to_vec()], 1, 0)
        .expect("install single-member steward list");
    service
}
