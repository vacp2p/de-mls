//! Crate-internal test fixtures: minimal trait impls for tests that need
//! to construct a [`crate::core::ConversationHandle`] or [`crate::app::SessionRunner`]
//! without standing up real MLS / scoring / steward backends.
//!
//! Most methods are `unreachable!()` — tests should only exercise the
//! handful of paths the test specifically targets. If a test reaches
//! into an `unreachable!()` branch, that's a sign the test is touching
//! state it shouldn't be (and the panic location pinpoints the leak).

use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;

use crate::{
    app::{ConsensusContext, ConversationConfig, User, UserPlugins},
    core::{
        ConsensusPlugin, ConversationPluginsFactory, ElectionDecision, PeerScoringEvent,
        PeerScoringPlugin, PluginConsensus, ScoreOp, ScoreSnapshot, ScoringConfig, StewardList,
        StewardListConfig, StewardListEvent, StewardListPlugin,
    },
    defaults::{DefaultConsensusPlugin, DefaultConversationPluginsFactory, MemoryDeMlsStorage},
    ds::{OutboundPacket, SharedDeliveryService},
    identity::Identity,
    mls_crypto::{
        CommitCandidate, DecryptResult, MlsCommitInput, MlsCredentials, MlsError, MlsMessageKind,
        MlsService, StagedCandidateResult,
    },
    protos::de_mls::messages::v1::AppMessage,
};

/// Wallet-flavoured test `Identity`: 20-byte Ethereum address + EIP-55 hex.
pub(crate) struct TestWalletIdentity {
    bytes: Vec<u8>,
    display: String,
}

impl TestWalletIdentity {
    pub(crate) fn from_private_key(pk: &str) -> (Self, PrivateKeySigner) {
        let signer = PrivateKeySigner::from_str(pk).expect("valid private key");
        let addr: Address = signer.address();
        let identity = Self {
            bytes: addr.as_slice().to_vec(),
            display: addr.to_checksum(None),
        };
        (identity, signer)
    }
}

impl Identity for TestWalletIdentity {
    fn identity_bytes(&self) -> &[u8] {
        &self.bytes
    }
    fn identity_display(&self) -> &str {
        &self.display
    }
}

/// Test helper: build a `User` keyed by an Ethereum private key. Mirrors
/// what `tests/common/wallet.rs::user_from_private_key` does for the
/// integration suite — kept here so crate-internal tests can construct a
/// default-bundle `User` without a `tests/common` import.
pub(crate) fn make_user_from_private_key(
    private_key: &str,
    transport: SharedDeliveryService,
) -> User<DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
    let (identity, signer) = TestWalletIdentity::from_private_key(private_key);

    let credentials = Arc::new(MlsCredentials::from_identity(&identity).expect("credentials"));
    let storage = Arc::new(MemoryDeMlsStorage::new());
    let conversation_plugins = DefaultConversationPluginsFactory::new(storage, credentials);

    let consensus_signer = EthereumConsensusSigner::new(signer);
    let consensus = ConsensusContext::<DefaultConsensusPlugin>::new(consensus_signer);

    let plugins = UserPlugins {
        conversation_plugins,
        consensus,
        default_conversation_config: ConversationConfig::default(),
        default_scoring_config: ScoringConfig::default(),
        default_steward_list_config: StewardListConfig::default(),
    };

    User::new_with_plugins(Box::new(identity), plugins, transport)
}

/// Build a `PluginConsensus<DefaultConsensusPlugin>` with a random signer for
/// tests that need a `SessionRunner` but never exercise consensus operations.
pub(crate) fn make_test_consensus_service() -> PluginConsensus<DefaultConsensusPlugin> {
    PluginConsensus::<DefaultConsensusPlugin>::new_with_components(
        DefaultConsensusPlugin::new_storage(),
        DefaultConsensusPlugin::new_event_bus(),
        EthereumConsensusSigner::new(PrivateKeySigner::random()),
        10,
    )
}

/// Transport stub for tests. `publish` is unreachable (tests should never
/// push outbound) and `subscribe` is a no-op.
#[derive(Debug)]
pub(crate) struct UnusedTransport;

impl crate::ds::DeliveryService for UnusedTransport {
    type Error = crate::ds::DeliveryServiceError;

    fn publish(&mut self, _: crate::ds::OutboundPacket) -> Result<(), Self::Error> {
        unreachable!("UnusedTransport::publish called")
    }

    fn subscribe(&mut self, _delivery_address: &str) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// MLS service that errors on every operation. Lets tests construct a
/// `ConversationHandle` whose early-return paths never invoke MLS.
pub(crate) struct UnusedMls;

impl MlsService for UnusedMls {
    fn conversation_id(&self) -> &str {
        "unused"
    }
    fn delete(&mut self) -> Result<(), MlsError> {
        unreachable!("UnusedMls::delete called")
    }
    fn members(&self) -> Result<Vec<Vec<u8>>, MlsError> {
        unreachable!("UnusedMls::members called")
    }
    fn is_member(&self, _: &[u8]) -> bool {
        unreachable!("UnusedMls::is_member called")
    }
    fn current_epoch(&self) -> Result<u64, MlsError> {
        unreachable!("UnusedMls::current_epoch called")
    }
    fn create_commit_candidate(
        &mut self,
        _: &[MlsCommitInput],
    ) -> Result<CommitCandidate, MlsError> {
        unreachable!("UnusedMls::create_commit_candidate called")
    }
    fn merge_own_commit(&mut self) -> Result<(), MlsError> {
        unreachable!()
    }
    fn discard_own_commit(&mut self) -> Result<(), MlsError> {
        unreachable!()
    }
    fn stage_remote_commit(
        &mut self,
        _: &[Vec<u8>],
        _: &[u8],
    ) -> Result<StagedCandidateResult, MlsError> {
        unreachable!()
    }
    fn merge_staged_commit(&mut self) -> Result<(), MlsError> {
        unreachable!()
    }
    fn discard_staged_commit(&mut self) -> Result<(), MlsError> {
        unreachable!()
    }
    fn encrypt(&mut self, _: &[u8]) -> Result<Vec<u8>, MlsError> {
        unreachable!()
    }
    fn build_message(&mut self, _: &AppMessage, _: &[u8]) -> Result<OutboundPacket, MlsError> {
        unreachable!()
    }
    fn decrypt_application_only(&mut self, _: &[u8]) -> Result<DecryptResult, MlsError> {
        unreachable!()
    }
    fn decrypt(&mut self, _: &[u8]) -> Result<DecryptResult, MlsError> {
        unreachable!()
    }
    fn inspect_message_kind(&self, _: &[u8]) -> Result<MlsMessageKind, MlsError> {
        unreachable!()
    }
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
    fn retry_round(&self) -> u32 {
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
    ) -> Result<Vec<StewardListEvent>, crate::core::CoreError> {
        unreachable!()
    }
    fn maybe_auto_fill(
        &mut self,
        _: u64,
        _: &[Vec<u8>],
    ) -> Result<Vec<StewardListEvent>, crate::core::CoreError> {
        unreachable!()
    }
    fn validate_proposed(
        &self,
        _: &[Vec<u8>],
        _: u64,
        _: &[Vec<u8>],
        _: u32,
    ) -> Result<bool, crate::core::CoreError> {
        unreachable!()
    }
    fn propose_election<F: Fn(&[u8]) -> bool>(
        &self,
        _: u64,
        _: &[Vec<u8>],
        _: &[u8],
        _: F,
        _: bool,
    ) -> Result<ElectionDecision, crate::core::CoreError> {
        unreachable!()
    }
    fn bump_retry(&mut self) -> Vec<StewardListEvent> {
        unreachable!()
    }
    fn reset_retry(&mut self) {
        unreachable!()
    }
}

/// Scoring plug-in that panics on every call. Tests that don't read
/// scores use this as `StubPluginsFactory::Scoring` so the bundle still
/// satisfies [`crate::core::ConversationPluginsFactory`].
pub(crate) struct StubScoring;

impl PeerScoringPlugin for StubScoring {
    fn add_member(&mut self, _: &[u8]) -> Vec<PeerScoringEvent> {
        unreachable!()
    }
    fn remove_member(&mut self, _: &[u8]) {
        unreachable!()
    }
    fn apply_op(&mut self, _: &ScoreOp) -> Vec<PeerScoringEvent> {
        unreachable!()
    }
    fn apply_snapshot(&mut self, _: &ScoreSnapshot) -> Vec<PeerScoringEvent> {
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
    fn set_default_score(&mut self, _: i64) {
        unreachable!()
    }
}

/// Test plug-in bundle wiring the three stubs into the [`ConversationPluginsFactory`]
/// trait so tests can construct [`crate::core::ConversationHandle`] and
/// [`crate::app::SessionRunner`] under their single `<CP>` parameter. The
/// factory methods are `unreachable!()` — tests build plug-in instances
/// directly and hand them to the handle/runner constructors.
pub(crate) struct StubPluginsFactory;

impl ConversationPluginsFactory for StubPluginsFactory {
    type Mls = UnusedMls;
    type Scoring = StubScoring;
    type StewardList = StubStewardList;

    fn create_mls(&self, _: String) -> Result<Self::Mls, MlsError> {
        unreachable!()
    }
    fn welcome_mls(&self, _: &[u8]) -> Result<Option<Self::Mls>, MlsError> {
        unreachable!()
    }
    fn make_scoring(&self, _: &ScoringConfig) -> Self::Scoring {
        unreachable!()
    }
    fn make_steward_list(&self, _: &[u8], _: StewardListConfig) -> Self::StewardList {
        unreachable!()
    }
    fn generate_key_package(&self) -> Result<crate::mls_crypto::KeyPackageBytes, MlsError> {
        unreachable!()
    }
}
