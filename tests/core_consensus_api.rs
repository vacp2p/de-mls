//! Compile-time verification that the core consensus API is usable by external consumers.
//!
//! This test doesn't run real consensus — it just proves that all types, functions,
//! and trait bounds are publicly accessible from `de_mls::core` without touching `app`.

use de_mls::core::{
    // Consensus functions
    cast_vote,
    dispatch_result,
    forward_incoming_proposal,
    forward_incoming_vote,
    handle_consensus_event,
    start_voting,
    // Core types
    CoreError,
    DeMlsProvider,
    DefaultProvider,
    // Consensus return type
    DispatchAction,
    GroupEventHandler,
    GroupHandle,
    ProcessResult,
};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService};

// Verify that the re-exported consensus types are nameable
fn _assert_dispatch_action_variants(action: DispatchAction) {
    match action {
        DispatchAction::Done => {}
        DispatchAction::StartVoting(_request) => {}
        DispatchAction::GroupUpdated => {}
        DispatchAction::LeaveGroup => {}
        DispatchAction::JoinedGroup => {}
    }
}

/// Verify that `start_voting` is callable with `DefaultProvider` type params.
/// We only check that the function signature compiles — not that it runs.
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

/// Verify `handle_consensus_event` compiles.
async fn _check_handle_consensus_event(
    handle: &mut GroupHandle,
    event: hashgraph_like_consensus::types::ConsensusEvent,
    consensus: &<DefaultProvider as DeMlsProvider>::Consensus,
) -> Result<(), CoreError> {
    handle_consensus_event::<DefaultProvider>(handle, "group", event, consensus).await
}

/// Verify `dispatch_result` compiles and returns `DispatchAction`.
async fn _check_dispatch_result(
    handle: &GroupHandle,
    result: ProcessResult,
    consensus: &<DefaultProvider as DeMlsProvider>::Consensus,
    handler: &dyn GroupEventHandler,
    mls: &MlsService<MemoryDeMlsStorage>,
) -> Result<DispatchAction, CoreError> {
    dispatch_result::<DefaultProvider, _>(handle, "group", result, consensus, handler, mls).await
}

/// Dummy test so `cargo test` picks up this file and compiles it.
#[test]
fn core_consensus_api_is_public() {
    // If this file compiles, the API surface is accessible.
}
