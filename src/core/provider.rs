//! Service provider trait for DE-MLS.
//!
//! The [`DeMlsProvider`] trait bundles all configurable services needed
//! by the DE-MLS system into a single type parameter. This avoids threading
//! five separate generic parameters throughout the codebase.
//!
//! # Example
//!
//! ```ignore
//! use de_mls::core::{DeMlsProvider, DefaultProvider};
//! use de_mls::app::User;
//!
//! // Use the default provider (OpenMLS + hashgraph consensus)
//! let user: User<DefaultProvider> = User::with_private_key("0x...", consensus)?;
//!
//! // Or define your own provider
//! struct MyProvider;
//! impl DeMlsProvider for MyProvider {
//!     type Identity = MyIdentityService;
//!     // ...
//! }
//! let user: User<MyProvider> = User::new(my_identity, my_consensus, signer);
//! ```

use hashgraph_like_consensus::{
    api::ConsensusServiceAPI,
    events::{BroadcastEventBus, ConsensusEventBus},
    scope::ConsensusScope,
    service::DefaultConsensusService,
    storage::{ConsensusStorage, InMemoryConsensusStorage},
};
use mls_crypto::{IdentityService, MlsGroupService, OpenMlsIdentityService};

/// Bundles all configurable services for a DE-MLS deployment.
///
/// Implement this trait to provide custom identity, MLS, or consensus
/// implementations. Use [`DefaultProvider`] for the standard configuration.
pub trait DeMlsProvider: 'static {
    /// Identity and MLS service (e.g., `OpenMlsIdentityService`).
    type Identity: IdentityService + MlsGroupService + Send + Sync + 'static;

    /// Consensus scope type (typically `String` — the group name).
    type Scope: ConsensusScope + From<String> + Send + Sync + 'static;

    /// Consensus storage backend.
    type Storage: ConsensusStorage<Self::Scope> + Send + Sync + 'static;

    /// Consensus event bus.
    type EventBus: ConsensusEventBus<Self::Scope> + Send + Sync + 'static;

    /// Consensus service implementing the full API.
    type Consensus: ConsensusServiceAPI<Self::Scope, Self::Storage, Self::EventBus>
        + Send
        + Sync
        + 'static;
}

/// Default provider using OpenMLS identity, hashgraph consensus, and Waku delivery.
pub struct DefaultProvider;

impl DeMlsProvider for DefaultProvider {
    type Identity = OpenMlsIdentityService;
    type Scope = String;
    type Storage = InMemoryConsensusStorage<String>;
    type EventBus = BroadcastEventBus<String>;
    type Consensus = DefaultConsensusService;
}
