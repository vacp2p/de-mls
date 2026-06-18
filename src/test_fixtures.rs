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
    ConsensusPlugin, ConsensusServiceFor, ElectionDecision, PeerScoringPlugin, ScoreOp,
    ScoreSnapshot, StewardList, StewardListConfig, StewardListPlugin,
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

/// Steward-list plug-in with controllable `is_steward`. Other methods panic.
pub(crate) struct StubStewardList {
    is_steward: bool,
    config: StewardListConfig,
}

impl StubStewardList {
    pub(crate) fn member() -> Self {
        Self {
            is_steward: false,
            config: StewardListConfig::new(1, 5).unwrap(),
        }
    }
    pub(crate) fn steward() -> Self {
        Self {
            is_steward: true,
            config: StewardListConfig::new(1, 5).unwrap(),
        }
    }
}

impl StewardListPlugin for StubStewardList {
    fn config(&self) -> &StewardListConfig {
        &self.config
    }
    fn set_config(&mut self, _: StewardListConfig) {
        unreachable!()
    }
    fn current_list(&self) -> Option<&StewardList> {
        None
    }
    fn election_epoch(&self) -> Option<u64> {
        None
    }
    fn next_retry_round(&self) -> u32 {
        0
    }
    fn max_retries(&self) -> u32 {
        0
    }
    fn set_max_retries(&mut self, _: u32) {}
    fn is_steward(&self, _: &[u8]) -> bool {
        self.is_steward
    }
    fn is_exhausted(&self, _: u64) -> bool {
        unreachable!()
    }
    fn epoch_steward<F: Fn(&[u8]) -> bool>(&self, _: u64, _: F) -> Option<&[u8]> {
        unreachable!()
    }
    fn epoch_and_backup<F: Fn(&[u8]) -> bool>(
        &self,
        _: u64,
        _: F,
    ) -> (Option<&[u8]>, Option<&[u8]>) {
        unreachable!()
    }
    fn steward_members<F: Fn(&[u8]) -> bool>(&self, _: F) -> Vec<Vec<u8>> {
        unreachable!()
    }
    fn election_proposer<F: Fn(&[u8]) -> bool>(&self, _: F) -> Option<&[u8]> {
        unreachable!()
    }
    fn install_list(
        &mut self,
        _: u64,
        _: &[Vec<u8>],
        _: usize,
        _: u32,
    ) -> Result<(), crate::ConversationError> {
        unreachable!()
    }
    fn validate_proposed(
        &self,
        _: &[Vec<u8>],
        _: u64,
        _: &[Vec<u8>],
        _: u32,
    ) -> Result<bool, crate::ConversationError> {
        unreachable!()
    }
    fn propose_election<F: Fn(&[u8]) -> bool>(
        &self,
        _: u64,
        _: &[Vec<u8>],
        _: &[u8],
        _: F,
        _: bool,
    ) -> Result<ElectionDecision, crate::ConversationError> {
        unreachable!()
    }
    fn bump_retry(&mut self) {
        unreachable!()
    }
    fn reset_retry(&mut self) {
        unreachable!()
    }
}

/// Scoring plug-in that panics on every call. Tests that don't read
/// scores hand this to the conversation so the unread scoring slot is
/// inert.
pub(crate) struct StubScoring;

impl PeerScoringPlugin for StubScoring {
    fn add_member(&mut self, _: &[u8]) -> bool {
        unreachable!()
    }
    fn remove_member(&mut self, _: &[u8]) {
        unreachable!()
    }
    fn apply_op(&mut self, _: &ScoreOp) -> bool {
        unreachable!()
    }
    fn apply_snapshot(&mut self, _: &ScoreSnapshot) -> bool {
        unreachable!()
    }
    fn snapshot(&self) -> ScoreSnapshot {
        unreachable!()
    }
    fn score_for(&self, _: &[u8]) -> Option<i64> {
        unreachable!()
    }
    fn members_below_threshold(&self) -> Vec<Vec<u8>> {
        unreachable!()
    }
    fn all_members_with_scores(&self) -> Vec<(Vec<u8>, i64)> {
        unreachable!()
    }
    fn threshold(&self) -> i64 {
        unreachable!()
    }
    fn set_threshold(&mut self, _: i64) {
        unreachable!()
    }
    fn default_score(&self) -> i64 {
        unreachable!()
    }
}
