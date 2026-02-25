//! Compile-time verification that the core API is usable by external consumers.
//!
//! This test doesn't run real consensus — it just proves that all types, functions,
//! and trait bounds are publicly accessible from `de_mls::core` without touching `app`.

use de_mls::core::{
    // Consensus types
    ConsensusOutcome,
    // Core types
    CoreError,
    DeMlsProvider,
    DefaultProvider,
    GroupEventHandler,
    GroupHandle,
    ProcessResult,
    QuarantinePolicy,
    // Pure core functions
    apply_consensus_result,
    // Consensus convenience ops (still public)
    cast_vote,
    forward_incoming_proposal,
    forward_incoming_vote,
    has_quarantined,
    quarantine_len,
    retry_quarantined,
    start_voting,
};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService};

// Verify that ConsensusOutcome variants are nameable
fn _assert_consensus_outcome_variants(outcome: ConsensusOutcome<'_>) {
    match outcome {
        ConsensusOutcome::Approved { payload: _ } => {}
        ConsensusOutcome::ApprovedOwner => {}
        ConsensusOutcome::Rejected => {}
    }
}

/// Verify that `apply_consensus_result` signature compiles.
fn _check_apply_consensus_result(handle: &mut GroupHandle) -> Result<(), CoreError> {
    apply_consensus_result(handle, 1, ConsensusOutcome::Rejected)
}

/// Verify `retry_quarantined` returns Vec<ProcessResult>.
fn _check_retry_quarantined(
    handle: &mut GroupHandle,
    mls: &MlsService<MemoryDeMlsStorage>,
) -> Result<Vec<ProcessResult>, CoreError> {
    retry_quarantined(handle, mls)
}

/// Verify `quarantine_len` and `has_quarantined` compile.
fn _check_quarantine_queries(handle: &GroupHandle) -> (usize, bool) {
    (quarantine_len(handle), has_quarantined(handle))
}

/// Verify `QuarantinePolicy` is constructible with Default.
fn _check_quarantine_policy() -> QuarantinePolicy {
    QuarantinePolicy::default()
}

/// Verify that `start_voting` is callable with `DefaultProvider` type params.
async fn _check_start_voting(
    consensus: &<DefaultProvider as DeMlsProvider>::Consensus,
    handler: &dyn GroupEventHandler,
) -> Result<u32, CoreError> {
    let request = de_mls::protos::de_mls::messages::v1::GroupUpdateRequest { payload: None };
    start_voting::<DefaultProvider>("group", &request, 3, "0xabc".into(), consensus, handler).await
}

/// Verify `forward_incoming_proposal` compiles.
async fn _check_forward_incoming_proposal(
    proposal: hashgraph_like_consensus::protos::consensus::v1::Proposal,
    consensus: &<DefaultProvider as DeMlsProvider>::Consensus,
    handler: &dyn GroupEventHandler,
) -> Result<(), CoreError> {
    forward_incoming_proposal::<DefaultProvider>("group", proposal, consensus, handler).await
}

/// Verify `forward_incoming_vote` compiles.
async fn _check_forward_incoming_vote(
    vote: hashgraph_like_consensus::protos::consensus::v1::Vote,
    consensus: &<DefaultProvider as DeMlsProvider>::Consensus,
) -> Result<(), CoreError> {
    forward_incoming_vote::<DefaultProvider>("group", vote, consensus).await
}

/// Verify `cast_vote` compiles with a concrete signer type.
async fn _check_cast_vote(
    handle: &GroupHandle,
    consensus: &<DefaultProvider as DeMlsProvider>::Consensus,
    signer: alloy::signers::local::PrivateKeySigner,
    mls: &MlsService<MemoryDeMlsStorage>,
    handler: &dyn GroupEventHandler,
) -> Result<(), CoreError> {
    cast_vote::<DefaultProvider, _, _>(handle, "group", 1, true, consensus, signer, mls, handler)
        .await
}

/// Dummy test so `cargo test` picks up this file and compiles it.
#[test]
fn core_api_is_public() {
    // If this file compiles, the API surface is accessible.
    // Verify removed APIs are absent — normal compile breakage is sufficient.
}
