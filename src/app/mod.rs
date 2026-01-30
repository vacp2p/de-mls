//! Example application layer for DE-MLS.
//!
//! This module provides a reference implementation showing how to use the core
//! library. It includes:
//!
//! - [`User`] - Manages multiple groups via `HashMap<String, GroupHandle>`
//! - [`GroupStateMachine`] - State transitions for steward epochs
//! - [`ConsensusHandler`] - Orchestrates consensus events
//! - [`PendingBatches`] - Stores batch proposals for non-stewards
//!
//! # Usage
//!
//! You can use this layer directly for typical applications, or use it as a
//! reference for building custom group management.
//!
//! ```ignore
//! use de_mls::app::User;
//!
//! let user = User::new(identity_service, consensus_service, signer);
//! user.create_group("my-group", true).await?;
//! user.send_message("my-group", message).await?;
//! ```

mod bootstrap;
mod consensus_handler;
mod group_registry;
mod pending_batches;
mod state_machine;
mod user;

pub use bootstrap::{
    bootstrap_core, bootstrap_core_from_env, create_user_instance, AppState, Bootstrap,
    BootstrapConfig, CoreCtx,
};
pub use consensus_handler::ConsensusHandler;
pub use group_registry::GroupRegistry;
pub use pending_batches::PendingBatches;
pub use state_machine::{GroupState, GroupStateMachine};
pub use user::{User, UserAction, UserError};
