//! Crate-internal test fixtures: minimal trait impls for tests that need
//! to construct a [`crate::core::ConversationHandle`] or [`crate::app::SessionRunner`]
//! without standing up real MLS / scoring / steward backends.
//!
//! Most methods are `unreachable!()` — tests should only exercise the
//! handful of paths the test specifically targets. If a test reaches
//! into an `unreachable!()` branch, that's a sign the test is touching
//! state it shouldn't be (and the panic location pinpoints the leak).

use crate::{
    core::{
        ElectionDecision, PeerScoringEvent, PeerScoringPlugin, ScoreOp, ScoreSnapshot, StewardList,
        StewardListConfig, StewardListEvent, StewardListPlugin,
    },
    ds::OutboundPacket,
    mls_crypto::{
        CommitCandidate, DecryptResult, MlsCommitInput, MlsError, MlsMessageKind, MlsService,
        StagedCandidateResult,
    },
    protos::de_mls::messages::v1::AppMessage,
};

/// MLS service that errors on every operation. Lets tests construct a
/// `ConversationHandle` whose early-return paths never invoke MLS.
pub(crate) struct UnusedMls;

impl MlsService for UnusedMls {
    fn conversation_id(&self) -> &str {
        "unused"
    }
    fn delete(&self) -> Result<(), MlsError> {
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
    fn create_commit_candidate(&self, _: &[MlsCommitInput]) -> Result<CommitCandidate, MlsError> {
        unreachable!("UnusedMls::create_commit_candidate called")
    }
    fn merge_own_commit(&self) -> Result<(), MlsError> {
        unreachable!()
    }
    fn discard_own_commit(&self) -> Result<(), MlsError> {
        unreachable!()
    }
    fn stage_remote_commit(
        &self,
        _: &[Vec<u8>],
        _: &[u8],
    ) -> Result<StagedCandidateResult, MlsError> {
        unreachable!()
    }
    fn merge_staged_commit(&self) -> Result<(), MlsError> {
        unreachable!()
    }
    fn discard_staged_commit(&self) -> Result<(), MlsError> {
        unreachable!()
    }
    fn encrypt(&self, _: &[u8]) -> Result<Vec<u8>, MlsError> {
        unreachable!()
    }
    fn build_message(&self, _: &AppMessage, _: &[u8]) -> Result<OutboundPacket, MlsError> {
        unreachable!()
    }
    fn decrypt_application_only(&self, _: &[u8]) -> Result<DecryptResult, MlsError> {
        unreachable!()
    }
    fn decrypt(&self, _: &[u8]) -> Result<DecryptResult, MlsError> {
        unreachable!()
    }
    fn inspect_message_kind(&self, _: &[u8]) -> Result<MlsMessageKind, MlsError> {
        unreachable!()
    }
}

/// Steward plug-in with controllable `is_steward`. Other methods panic.
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
    fn epoch_steward(&self, _: u64, _: &dyn Fn(&[u8]) -> bool) -> Option<&[u8]> {
        unreachable!()
    }
    fn epoch_and_backup(
        &self,
        _: u64,
        _: &dyn Fn(&[u8]) -> bool,
    ) -> (Option<&[u8]>, Option<&[u8]>) {
        unreachable!()
    }
    fn steward_members(&self, _: &dyn Fn(&[u8]) -> bool) -> Vec<Vec<u8>> {
        unreachable!()
    }
    fn election_proposer(&self, _: &dyn Fn(&[u8]) -> bool) -> Option<&[u8]> {
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
    fn propose_election(
        &self,
        _: u64,
        _: &[Vec<u8>],
        _: &[u8],
        _: &dyn Fn(&[u8]) -> bool,
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
/// scores at all use this to satisfy the `Sc: PeerScoringPlugin` bound.
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
