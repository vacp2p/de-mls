//! Service provider trait for dependency injection.
//!
//! The [`DeMlsProvider`] trait bundles all configurable services needed
//! by DE-MLS into a single type parameter. This enables swapping out
//! implementations for testing or custom deployments.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                      DeMlsProvider                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  Identity   │  Wallet signing + MLS key management          │
//! │  Scope      │  Group identifier type (usually String)       │
//! │  Storage    │  Consensus proposal/vote persistence          │
//! │  EventBus   │  Consensus event distribution                 │
//! │  Consensus  │  Voting service (hashgraph-like)              │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Default vs Custom Providers
//!
//! Use [`DefaultProvider`] for production deployments:
//! - `OpenMlsIdentityService` for Ethereum wallet + MLS
//! - `InMemoryConsensusStorage` for proposal/vote storage
//! - `BroadcastEventBus` for consensus event distribution
//! - `DefaultConsensusService` for hashgraph-like voting
//!
//! Create custom providers for:
//! - **Testing**: Mock services that don't require network
//! - **Persistence**: Database-backed consensus storage
//! - **Custom identity**: Different signing schemes
//!
//! # Example
//!
//! ```ignore
//! use de_mls::core::{DeMlsProvider, DefaultProvider};
//! use de_mls::app::User;
//!
//! // Production: use DefaultProvider
//! let user: User<DefaultProvider> = User::with_private_key(
//!     "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
//!     consensus,
//!     handler,
//! )?;
//!
//! // Testing: use a custom provider with mocks
//! struct TestProvider;
//! impl DeMlsProvider for TestProvider {
//!     type Identity = MockIdentityService;
//!     type Scope = String;
//!     type Storage = InMemoryConsensusStorage<String>;
//!     type EventBus = BroadcastEventBus<String>;
//!     type Consensus = MockConsensusService;
//! }
//! ```

use hashgraph_like_consensus::{
    api::ConsensusServiceAPI,
    events::{BroadcastEventBus, ConsensusEventBus},
    scope::ConsensusScope,
    service::DefaultConsensusService,
    storage::{ConsensusStorage, InMemoryConsensusStorage},
};

use crate::mls_crypto::{IdentityService, MlsGroupService, OpenMlsIdentityService};

/// Bundles all configurable services for a DE-MLS deployment.
///
/// This trait uses associated types to define the concrete implementations
/// of each service. All types must be `Send + Sync + 'static` for async use.
///
/// # Implementing Custom Providers
///
/// ```ignore
/// struct MyProvider;
///
/// impl DeMlsProvider for MyProvider {
///     // Your wallet/signing service
///     type Identity = MyIdentityService;
///
///     // Group identifier (String works for most cases)
///     type Scope = String;
///
///     // Where to store proposals and votes
///     type Storage = PostgresConsensusStorage<String>;
///
///     // How to distribute consensus events
///     type EventBus = BroadcastEventBus<String>;
///
///     // The voting service implementation
///     type Consensus = DefaultConsensusService;
/// }
/// ```
pub trait DeMlsProvider: 'static {
    /// Identity and MLS service for key management and signing.
    ///
    /// Must implement:
    /// - `IdentityService`: Wallet address, signing, key packages
    /// - `MlsGroupService`: Group creation, joining, message encryption
    ///
    /// Default: `OpenMlsIdentityService` (Ethereum wallet + OpenMLS)
    type Identity: IdentityService + MlsGroupService + Send + Sync + 'static;

    /// Consensus scope type for grouping proposals.
    ///
    /// This is typically `String` (the group name). Each group has its
    /// own consensus scope, so proposals don't interfere across groups.
    ///
    /// Default: `String`
    type Scope: ConsensusScope + From<String> + Send + Sync + 'static;

    /// Consensus storage backend for proposals and votes.
    ///
    /// Stores pending proposals, vote tallies, and consensus state.
    /// Can be in-memory for testing or database-backed for production.
    ///
    /// Default: `InMemoryConsensusStorage<String>`
    type Storage: ConsensusStorage<Self::Scope> + Send + Sync + 'static;

    /// Event bus for distributing consensus outcomes.
    ///
    /// When consensus is reached or fails, events are broadcast to
    /// all subscribers (typically the app layer's event loop).
    ///
    /// Default: `BroadcastEventBus<String>`
    type EventBus: ConsensusEventBus<Self::Scope> + Send + Sync + 'static;

    /// Consensus service implementing the voting protocol.
    ///
    /// Handles proposal creation, vote casting, and outcome determination.
    /// Uses hashgraph-like consensus for Byzantine fault tolerance.
    ///
    /// Default: `DefaultConsensusService`
    type Consensus: ConsensusServiceAPI<Self::Scope, Self::Storage, Self::EventBus>
        + Send
        + Sync
        + 'static;
}

/// Default provider for production deployments.
///
/// Uses:
/// - `OpenMlsIdentityService`: Ethereum wallet signing + OpenMLS key management
/// - `String` scope: Group names as consensus scopes
/// - `InMemoryConsensusStorage`: Fast in-memory proposal/vote storage
/// - `BroadcastEventBus`: Tokio broadcast channels for events
/// - `DefaultConsensusService`: Hashgraph-like voting protocol
///
/// # Example
///
/// ```ignore
/// use de_mls::core::DefaultProvider;
/// use de_mls::app::User;
///
/// let user: User<DefaultProvider> = User::with_private_key(
///     private_key,
///     consensus,
///     handler,
/// )?;
/// ```
pub struct DefaultProvider;

impl DeMlsProvider for DefaultProvider {
    type Identity = OpenMlsIdentityService;
    type Scope = String;
    type Storage = InMemoryConsensusStorage<String>;
    type EventBus = BroadcastEventBus<String>;
    type Consensus = DefaultConsensusService;
}
