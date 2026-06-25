//! Consensus backend contract.
//!
//! The integrator owns only two pieces of consensus infrastructure: durable
//! persistence and the vote-signing identity. Everything else — the scope key
//! (always the conversation id), the outcome bus, per-scope session capacity,
//! and every protocol call — is de-mls-internal.
//!
//! Reference implementation: [`crate::defaults::DefaultConsensusPlugin`].

use hashgraph_like_consensus::{
    scope::ScopeID, service::ConsensusService, signing::ConsensusSignatureScheme,
    storage::ConsensusStorage,
};

use crate::consensus::outcome_bus::OutcomeBus;

/// The consensus backend the integrator holds and hands to each conversation.
/// It picks the two concrete types the consensus library runs with — durable
/// storage and the vote-signing identity — and provides an instance of each.
/// de-mls fixes the scope to the conversation id and owns the outcome bus, so
/// neither appears here.
///
/// A single instance backs many conversations: [`storage`](Self::storage) is
/// called once per conversation, so an integrator sharing one scope-keyed store
/// returns a clone of it.
pub trait ConsensusPlugin {
    /// Proposal/vote persistence. Scope is always the conversation id, so the
    /// backend is keyed by [`ScopeID`].
    type ConsensusStorage: ConsensusStorage<ScopeID>;

    /// Signature scheme authenticating votes. All peers on a network must
    /// agree on this.
    type Signer: ConsensusSignatureScheme + Clone + 'static;

    /// The proposal/vote store for a new conversation.
    fn storage(&self) -> Self::ConsensusStorage;

    /// The vote-signing identity.
    fn signer(&self) -> Self::Signer;
}

/// The consensus engine de-mls runs per conversation: the integrator's storage
/// and signer over de-mls's fixed scope key and outcome bus. Built inside
/// [`crate::Conversation::create`] / `join`.
pub type ConsensusEngine<C> = ConsensusService<
    ScopeID,
    <C as ConsensusPlugin>::ConsensusStorage,
    OutcomeBus,
    <C as ConsensusPlugin>::Signer,
>;
