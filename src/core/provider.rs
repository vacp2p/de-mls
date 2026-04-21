//! Service provider trait for dependency injection.
//!
//! The [`DeMlsProvider`] trait bundles all configurable backends needed
//! by DE-MLS into a single type parameter. This enables swapping out
//! storage and event delivery for testing or custom deployments.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      DeMlsProvider                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Storage    │  MLS + DE-MLS state persistence               │
//! │  Scope      │  Group identifier type (usually String)       │
//! │  CStorage   │  Consensus proposal/vote persistence          │
//! │  EventBus   │  Consensus event distribution                 │
//! └─────────────────────────────────────────────────────────────┘
//!         │
//!         ▼ derives
//!   ProviderConsensus<P> = ConsensusService<Scope, CStorage, EventBus>
//! ```
//!
//! The consensus service type is **derived** from the provider's backend
//! types — there is no separate `type Consensus` because the voting logic
//! is fixed (hashgraph-like consensus). Only the backends (storage, event
//! bus) are swappable.
//!
//! # Default vs Custom Providers
//!
//! Use [`DefaultProvider`] for production deployments:
//! - `MemoryDeMlsStorage` for MLS state storage
//! - `InMemoryConsensusStorage` for proposal/vote storage
//! - `BroadcastEventBus` for consensus event distribution
//!
//! Create custom providers for:
//! - **Persistence**: Database-backed consensus storage
//! - **Event delivery**: Message queue instead of broadcast channel
//! - **Custom scope**: Non-string group identifiers
//!
//! # Example
//!
//! ```ignore
//! use de_mls::core::{DeMlsProvider, DefaultProvider, ProviderConsensus};
//! use de_mls::app::User;
//!
//! // Production: use DefaultProvider
//! let user: User<DefaultProvider, _, _> = User::with_private_key(
//!     "0xprivate_key",
//!     consensus,
//!     handler,
//!     state_handler,
//! )?;
//! ```

use hashgraph_like_consensus::{
    events::{BroadcastEventBus, ConsensusEventBus},
    scope::ConsensusScope,
    service::ConsensusService,
    storage::{ConsensusStorage, InMemoryConsensusStorage},
};

use openmls_rust_crypto::MemoryStorage;

use crate::mls_crypto::{DeMlsStorage, MemoryDeMlsStorage};

/// Bundles all configurable backends for a DE-MLS deployment.
///
/// The consensus service type is derived from the backend types via
/// [`ProviderConsensus<P>`] — you configure storage and event delivery,
/// and the consensus service follows automatically.
pub trait DeMlsProvider: 'static {
    /// Storage backend for MLS operations.
    ///
    /// Default: `MemoryDeMlsStorage`
    type Storage: DeMlsStorage<MlsStorage = MemoryStorage> + Send + Sync + 'static;

    /// Consensus scope type for grouping proposals.
    ///
    /// Each group has its own scope. Typically `String` (group name),
    /// but integrators may use `Vec<u8>`, UUIDs, etc.
    ///
    /// Default: `String`
    type Scope: ConsensusScope + From<String> + Send + Sync + 'static;

    /// Consensus storage backend for proposals and votes.
    ///
    /// Default: `InMemoryConsensusStorage<String>`
    type ConsensusStorage: ConsensusStorage<Self::Scope> + Send + Sync + 'static;

    /// Event bus for distributing consensus outcomes.
    ///
    /// Default: `BroadcastEventBus<String>`
    type EventBus: ConsensusEventBus<Self::Scope> + Send + Sync + 'static;
}

/// The consensus service type for a given provider.
///
/// Derived from the provider's Scope, ConsensusStorage, and EventBus.
/// The consensus logic is fixed — only the backends are swappable.
pub type ProviderConsensus<P> = ConsensusService<
    <P as DeMlsProvider>::Scope,
    <P as DeMlsProvider>::ConsensusStorage,
    <P as DeMlsProvider>::EventBus,
>;

/// Default provider using in-memory backends.
pub struct DefaultProvider;

impl DeMlsProvider for DefaultProvider {
    type Storage = MemoryDeMlsStorage;
    type Scope = String;
    type ConsensusStorage = InMemoryConsensusStorage<String>;
    type EventBus = BroadcastEventBus<String>;
}
