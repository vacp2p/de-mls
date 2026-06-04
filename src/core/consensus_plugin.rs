//! Consensus backend "plugin" contract.
//!
//! [`ConsensusPlugin`] pins the concrete types used by the consensus library:
//! - **`Scope`**: the per-conversation partition key
//! - **storage**: proposal/vote persistence
//! - **event bus**: delivery of consensus outcomes
//! - **signer**: vote authentication (must match across peers)
//!
//! The app layer constructs a per-conversation [`ConsensusServiceFor<P>`] using
//! these associated types and factories.
//!
//! Reference implementation: [`crate::defaults::DefaultConsensusPlugin`].

use hashgraph_like_consensus::{
    events::ConsensusEventBus, scope::ConsensusScope, service::ConsensusService,
    signing::ConsensusSignatureScheme, storage::ConsensusStorage, types::ConsensusEvent,
};

/// Pull-side contract for a [`ConsensusEventBus`] receiver: non-blocking.
///
/// Returns `None` when the queue is empty.
///
/// `SessionRunner` holds one receiver per session and drains it from
/// `tick_deadlines`.
pub trait SyncConsensusReceiver<Scope: ConsensusScope>: Send {
    fn try_recv(&mut self) -> Option<(Scope, ConsensusEvent)>;
}

/// User-level consensus backend bundle.
///
/// This trait only selects the concrete types the consensus library runs with:
/// scope key, storage, event bus, and signer. The app layer decides how to
/// instantiate and share those pieces across conversations, then builds a
/// per-conversation service via [`ConsensusServiceFor`].
pub trait ConsensusPlugin {
    /// Conversation identifier type used as a consensus scope key.
    type Scope: ConsensusScope + From<String>;

    /// Proposal/vote persistence.
    type ConsensusStorage: ConsensusStorage<Self::Scope>;

    /// Consensus-outcome delivery (event bus).
    ///
    /// The receiver must implement [`SyncConsensusReceiver`] so the app can
    /// drain it from a tick loop.
    type EventBus: ConsensusEventBus<Self::Scope, Receiver: SyncConsensusReceiver<Self::Scope>>;

    /// Signature scheme for authenticating votes.
    ///
    /// All peers on a network must agree on this.
    type Signer: ConsensusSignatureScheme + Clone + 'static;

    /// Build storage.
    fn new_storage() -> Self::ConsensusStorage;

    /// Build an event bus.
    ///
    /// Most app wiring uses one bus per conversation.
    fn new_event_bus() -> Self::EventBus;
}

/// Concrete consensus service derived from a [`ConsensusPlugin`]'s
/// associated types.
pub type ConsensusServiceFor<P> = ConsensusService<
    <P as ConsensusPlugin>::Scope,
    <P as ConsensusPlugin>::ConsensusStorage,
    <P as ConsensusPlugin>::EventBus,
    <P as ConsensusPlugin>::Signer,
>;
