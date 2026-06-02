//! [`ConsensusPlugin`] — user-level bundle of consensus-library backends
//! (scope key, proposal/vote storage, event bus). One plug-in per `User`
//! today; per-conversation consensus is on the roadmap and will expand
//! this surface. The concrete consensus service type is derived via
//! [`PluginConsensus<P>`] — the voting algorithm itself is fixed.
//!
//! Reference impl: [`crate::defaults::DefaultConsensusPlugin`].

use hashgraph_like_consensus::{
    events::ConsensusEventBus, scope::ConsensusScope, service::ConsensusService,
    signing::ConsensusSignatureScheme, storage::ConsensusStorage, types::ConsensusEvent,
};

/// Pull-side contract on an [`ConsensusEventBus`] receiver: non-blocking,
/// returns `None` when the queue is empty.
///
/// `SessionRunner` holds one receiver per session and drains it from
/// `tick_deadlines`.
pub trait SyncConsensusReceiver<Scope: ConsensusScope>: Send + 'static {
    fn try_recv(&mut self) -> Option<(Scope, ConsensusEvent)>;
}

/// User-level consensus backend bundle. Carries the four types the
/// `hashgraph_like_consensus` service is parameterised by: the scope key
/// (one consensus partition per conversation), proposal/vote storage,
/// outcome event bus, and the signature scheme used to authenticate votes.
/// The concrete service is materialised via [`PluginConsensus`].
pub trait ConsensusPlugin: 'static {
    /// Conversation-identifier type used as consensus scope (default: `String`).
    type Scope: ConsensusScope + From<String> + 'static;

    /// Proposal/vote persistence (default: `InMemoryConsensusStorage<String>`).
    type ConsensusStorage: ConsensusStorage<Self::Scope> + 'static;

    /// Consensus-outcome delivery (default:
    /// [`crate::defaults::SyncEventBus<String>`]). The receiver implements
    /// [`SyncConsensusReceiver`] so `SessionRunner` drains it from
    /// `tick_deadlines`.
    type EventBus: ConsensusEventBus<Self::Scope, Receiver: SyncConsensusReceiver<Self::Scope>>
        + 'static;

    /// Signature scheme for authenticating votes (default:
    /// [`hashgraph_like_consensus::signing::EthereumConsensusSigner`]).
    /// All peers on a network must agree.
    type Signer: ConsensusSignatureScheme + Clone + 'static;

    /// Build a fresh storage.
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
