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
    signing::{ConsensusSignatureScheme, EthereumConsensusSigner},
    storage::{ConsensusStorage, InMemoryConsensusStorage},
};

/// User-level consensus backend bundle. Carries the four types the
/// `hashgraph_like_consensus` service is parameterised by: the scope key
/// (one consensus partition per conversation), proposal/vote storage,
/// outcome event bus, and the signature scheme used to authenticate votes.
/// The concrete service is materialised via [`PluginConsensus`].
pub trait ConsensusPlugin: 'static {
    /// Conversation-identifier type used as consensus scope (default: `String`).
    type Scope: ConsensusScope + From<String> + Send + Sync + 'static;

    /// Proposal/vote persistence (default: `InMemoryConsensusStorage<String>`).
    type ConsensusStorage: ConsensusStorage<Self::Scope> + Send + Sync + 'static;

    /// Consensus-outcome delivery (default: `BroadcastEventBus<String>`).
    type EventBus: ConsensusEventBus<Self::Scope> + Send + Sync + 'static;

    /// Signature scheme for authenticating votes (default:
    /// [`EthereumConsensusSigner`]). All peers on a network must agree.
    type Signer: ConsensusSignatureScheme + Clone + Send + Sync + 'static;

    /// Build a fresh storage handle. Called once at `User` init; the handle
    /// is cloned per conversation so all per-conv `ConsensusService` instances
    /// share one underlying persistence (see upstream "per-scope service
    /// composition" pattern).
    fn new_storage() -> Self::ConsensusStorage;

    /// Build a fresh event bus for one conversation. Each per-conv
    /// `ConsensusService` owns its own bus; subscribers automatically see
    /// only that conversation's events.
    fn new_event_bus() -> Self::EventBus;
}

/// Concrete consensus service derived from a [`ConsensusPlugin`]'s
/// associated types.
pub type PluginConsensus<P> = ConsensusService<
    <P as ConsensusPlugin>::Scope,
    <P as ConsensusPlugin>::ConsensusStorage,
    <P as ConsensusPlugin>::EventBus,
    <P as ConsensusPlugin>::Signer,
>;

/// In-memory consensus plug-in suitable for tests and simple deployments.
pub struct DefaultConsensusPlugin;

impl ConsensusPlugin for DefaultConsensusPlugin {
    type Scope = String;
    type ConsensusStorage = InMemoryConsensusStorage<String>;
    type EventBus = BroadcastEventBus<String>;
    type Signer = EthereumConsensusSigner;

    fn new_storage() -> Self::ConsensusStorage {
        InMemoryConsensusStorage::new()
    }

    fn new_event_bus() -> Self::EventBus {
        BroadcastEventBus::default()
    }
}
