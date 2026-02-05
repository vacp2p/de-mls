//! Example application layer for DE-MLS.
//!
//! This module provides a reference implementation showing how to use the core
//! library. It includes:
//!
//! - [`User`] - Manages multiple groups via `HashMap<String, GroupHandle>`
//! - [`GroupStateMachine`] - State transitions for steward epochs
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

mod scheduler;
mod state_machine;
mod user;

pub use scheduler::{IntervalScheduler, StewardScheduler, StewardSchedulerConfig};
pub use state_machine::{GroupState, GroupStateMachine, StateChangeHandler};
pub use user::{User, UserError};
