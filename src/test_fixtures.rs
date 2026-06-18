//! Crate-internal test fixtures: minimal trait impls for tests that need
//! to construct a [`crate::session::Conversation`] without standing up real
//! MLS / scoring / steward backends.
//!
//! Most methods are `unreachable!()` — tests should only exercise the
//! handful of paths the test specifically targets. If a test reaches
//! into an `unreachable!()` branch, that's a sign the test is touching
//! state it shouldn't be (and the panic location pinpoints the leak).

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;
use openmls_traits::signatures::{Signer, SignerError};
use openmls_traits::types::SignatureScheme;

use crate::{
    core::{
        ConsensusPlugin, ConsensusServiceFor, ConversationPluginsFactory, ElectionDecision,
        PeerScoringPlugin, ScoreOp, ScoreSnapshot, ScoringConfig, StewardList, StewardListConfig,
        StewardListPlugin,
    },
    defaults::DefaultConsensusPlugin,
    mls_crypto::{
        CommitCandidate, DecryptResult, MlsCommitInput, MlsError, MlsMessageKind, MlsService,
        StagedCandidateResult,
    },
    protos::de_mls::messages::v1::AppMessage,
};

/// Build a `ConsensusServiceFor<DefaultConsensusPlugin>` paired with a
/// subscribed receiver for [`crate::session::Conversation::new`].
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

/// Signer that panics on use. Lets tests pass a signer through paths that
/// return before any signing happens.
pub(crate) struct UnusedSigner;

impl Signer for UnusedSigner {
    fn sign(&self, _: &[u8]) -> Result<Vec<u8>, SignerError> {
        unreachable!("UnusedSigner::sign called")
    }
    fn signature_scheme(&self) -> SignatureScheme {
        unreachable!("UnusedSigner::signature_scheme called")
    }
}

/// MLS service that errors on every operation. Lets tests construct a
/// `Conversation` whose early-return paths never invoke MLS.
pub(crate) struct UnusedMls;

impl MlsService for UnusedMls {
    fn conversation_id(&self) -> &str {
        "unused"
    }
    fn commit_batch_max(&self) -> usize {
        crate::mls_crypto::DEFAULT_COMMIT_BATCH_MAX
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
        _: &impl Signer,
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
    fn encrypt(&mut self, _: &impl Signer, _: &[u8]) -> Result<Vec<u8>, MlsError> {
        unreachable!()
    }
    fn build_message(&mut self, _: &impl Signer, _: &AppMessage) -> Result<Vec<u8>, MlsError> {
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
    ) -> Result<(), crate::core::CoreError> {
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
    fn bump_retry(&mut self) {
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

/// Test plug-in bundle wiring the three stubs into the [`ConversationPluginsFactory`]
/// trait so tests can construct a [`crate::session::Conversation`] under its
/// single `<CP>` parameter. The factory methods are `unreachable!()` — tests
/// build plug-in instances directly and hand them to the conversation
/// constructors.
pub(crate) struct StubPluginsFactory;

impl ConversationPluginsFactory for StubPluginsFactory {
    type Mls = UnusedMls;
    type Scoring = StubScoring;
    type StewardList = StubStewardList;

    fn create_mls(&self, _: String, _: &[u8], _: &impl Signer) -> Result<Self::Mls, MlsError> {
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
}
