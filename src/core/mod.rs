//! Core library for DE-MLS group management.
//!
//! This module provides the reusable core API for MLS group operations.
//! It is designed to be used by applications that want to build their own
//! group management on top of DE-MLS.
//!
//! # Key Components
//!
//! - [`GroupHandle`] - Per-group state container (MLS handle, app_id, steward)
//! - [`Steward`] - Simplified steward for proposal management (no crypto)
//! - [`GroupEventHandler`] - Trait for receiving callbacks from group operations
//! - Free function API for group lifecycle, messaging, and steward operations
//!
//! # Example
//!
//! ```ignore
//! use de_mls::core::{GroupHandle, GroupEventHandler, create_group, build_message};
//!
//! // Create a group
//! let handle = create_group("my-group", &identity_service)?;
//!
//! // Build and send a message
//! let packet = build_message(&handle, &identity_service, app_msg).await?;
//! handler.on_outbound("my-group", packet);
//! ```

mod api;
mod error;
mod events;
mod group_handle;
mod provider;
mod steward;
mod types;

pub use api::*;
pub use error::CoreError;
pub use events::GroupEventHandler;
pub use group_handle::GroupHandle;
pub use provider::{DeMlsProvider, DefaultProvider};
pub use steward::Steward;
pub use types::{
    convert_group_requests_to_display, message_types, GroupUpdateRequest, MessageType,
    ProcessResult,
};
