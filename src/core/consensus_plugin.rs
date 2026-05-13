//! [`ConsensusPlugin`] — user-level bundle of consensus-library backends
//! (scope key, proposal/vote storage, event bus). One plug-in per `User`
//! today; per-conversation consensus is on the roadmap and will expand
//! this surface. The concrete consensus service type is derived via
//! [`PluginConsensus<P>`] — the voting algorithm itself is fixed.
//! [`DefaultConsensusPlugin`] uses in-memory backends for tests and
//! simple deployments.

use hashgraph_like_consensus::{
    events::{BroadcastEventBus, ConsensusEventBus},
    scope::ConsensusScope,
    service::ConsensusService,
    storage::{ConsensusStorage, InMemoryConsensusStorage},
};

/// User-level consensus backend bundle. Carries the three types the
/// `hashgraph_like_consensus` service is parameterised by: the scope key
/// (one consensus partition per conversation), proposal/vote storage, and
/// the outcome event bus. The concrete service is materialised via
/// [`PluginConsensus`].
pub trait ConsensusPlugin: 'static {
    /// Conversation-identifier type used as consensus scope (default: `String`).
    type Scope: ConsensusScope + From<String> + Send + Sync + 'static;

    /// Proposal/vote persistence (default: `InMemoryConsensusStorage<String>`).
    type ConsensusStorage: ConsensusStorage<Self::Scope> + Send + Sync + 'static;

    /// Consensus-outcome delivery (default: `BroadcastEventBus<String>`).
    type EventBus: ConsensusEventBus<Self::Scope> + Send + Sync + 'static;
}

/// Concrete consensus service derived from a [`ConsensusPlugin`]'s
/// associated types.
pub type PluginConsensus<P> = ConsensusService<
    <P as ConsensusPlugin>::Scope,
    <P as ConsensusPlugin>::ConsensusStorage,
    <P as ConsensusPlugin>::EventBus,
>;

/// In-memory consensus plug-in suitable for tests and simple deployments.
pub struct DefaultConsensusPlugin;

impl ConsensusPlugin for DefaultConsensusPlugin {
    type Scope = String;
    type ConsensusStorage = InMemoryConsensusStorage<String>;
    type EventBus = BroadcastEventBus<String>;
}
