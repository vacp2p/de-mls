//! Core library for DE-MLS group management.
//!
//! This module provides the reusable core API for MLS group operations.
//! It is designed to be used by applications that want to build their own
//! group management on top of DE-MLS.
//!
//! # Key Components
//!
//! # Example
//!

mod api;
mod consensus;
mod error;
mod events;
mod group_handle;
mod group_update_handle;
mod provider;
mod types;

pub use api::*;
pub use consensus::*;
pub use error::CoreError;
pub use events::GroupEventHandler;
pub use group_handle::GroupHandle;
pub use group_update_handle::{CurrentEpochProposals, ProposalId};
pub use provider::{DeMlsProvider, DefaultProvider};
pub use types::{
    convert_group_request_to_display, get_identity_from_group_update_request, message_types,
    MessageType, ProcessResult,
};
