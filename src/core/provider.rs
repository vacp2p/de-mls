//! [`DeMlsProvider`] bundles the swappable backends (MLS storage, consensus
//! scope, consensus storage, event bus) so an integrator configures them
//! once at the app layer. The consensus service type is derived via
//! [`ProviderConsensus<P>`] — the voting algorithm itself is fixed.
//! [`DefaultProvider`] uses in-memory backends for tests and simple deployments.

use hashgraph_like_consensus::{
    events::{BroadcastEventBus, ConsensusEventBus},
    scope::ConsensusScope,
    service::ConsensusService,
    storage::{ConsensusStorage, InMemoryConsensusStorage},
};

use crate::mls_crypto::{DeMlsStorage, MemoryDeMlsStorage};

pub trait DeMlsProvider: 'static {
    /// MLS + DE-MLS state persistence (default: `MemoryDeMlsStorage`).
    type Storage: DeMlsStorage + Send + Sync + 'static;

    /// Conversation-identifier type used as consensus scope (default: `String`).
    type Scope: ConsensusScope + From<String> + Send + Sync + 'static;

    /// Proposal/vote persistence (default: `InMemoryConsensusStorage<String>`).
    type ConsensusStorage: ConsensusStorage<Self::Scope> + Send + Sync + 'static;

    /// Consensus-outcome delivery (default: `BroadcastEventBus<String>`).
    type EventBus: ConsensusEventBus<Self::Scope> + Send + Sync + 'static;
}

/// Consensus service type derived from the provider's backends.
pub type ProviderConsensus<P> = ConsensusService<
    <P as DeMlsProvider>::Scope,
    <P as DeMlsProvider>::ConsensusStorage,
    <P as DeMlsProvider>::EventBus,
>;

/// In-memory provider suitable for tests and simple deployments.
pub struct DefaultProvider;

impl DeMlsProvider for DefaultProvider {
    type Storage = MemoryDeMlsStorage;
    type Scope = String;
    type ConsensusStorage = InMemoryConsensusStorage<String>;
    type EventBus = BroadcastEventBus<String>;
}
