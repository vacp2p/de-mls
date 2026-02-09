//! Reference application layer for DE-MLS.
//!
//! This module provides a complete reference implementation showing how to build
//! on top of [`crate::core`]. You can use it directly or as a template for custom
//! implementations.
//!
//! # Overview
//!
//! The app layer adds:
//! - **`User`** - Multi-group manager with consensus integration
//! - **`GroupStateMachine`** - State transitions (PendingJoin → Working ⇄ Waiting → Leaving)
//! - **`StateChangeHandler`** - Callbacks for UI state updates
//! - **Epoch scheduling** - Configurable steward epoch timing
//!
//! # When to Use This Layer
//!
//! **Use directly** if you want:
//! - Standard chat functionality out of the box
//! - Epoch-based steward consensus model
//! - Built-in join timeout and leave handling
//!
//! **Build your own** if you need:
//! - Different consensus model (e.g., leader election)
//! - Custom state machine transitions
//! - Non-standard epoch timing
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │                    de_mls::app                          │
//! │                                                         │
//! │   User<P, H, S>                                         │
//! │   ├── groups: HashMap<String, GroupEntry>               │
//! │   │   └── GroupEntry                                    │
//! │   │       ├── handle: GroupHandle (from core)           │
//! │   │       └── state_machine: GroupStateMachine          │
//! │   ├── consensus_service                                 │
//! │   ├── handler: H (GroupEventHandler)                    │
//! │   └── state_handler: S (StateChangeHandler)             │
//! │                                                         │
//! │   GroupStateMachine                                     │
//! │   ├── state: PendingJoin | Working | Waiting | Leaving  │
//! │   ├── epoch timing (last_boundary, epoch_duration)      │
//! │   └── timeout tracking (pending_join, commit)           │
//! └─────────────────────────────────────────────────────────┘
//!                           │
//!                           ▼
//! ┌─────────────────────────────────────────────────────────┐
//! │                    de_mls::core                         │
//! │   GroupHandle, process_inbound, dispatch_result, ...    │
//! └─────────────────────────────────────────────────────────┘
//! ```
//!
//! # State Machine
//!
//! ```text
//!                    ┌──────────────┐
//!         join_group │ PendingJoin  │ timeout (2 epochs)
//!        ──────────► │              │ ──────────────────► cleanup
//!                    └──────┬───────┘
//!                           │ welcome received
//!                           ▼
//!                    ┌──────────────┐
//!                    │   Working    │◄─────────────────┐
//!                    │              │                  │
//!                    └──────┬───────┘                  │
//!                           │ epoch boundary           │ commit received
//!                           │ (has proposals)          │
//!                           ▼                          │
//!                    ┌──────────────┐                  │
//!                    │   Waiting    │──────────────────┘
//!                    │              │
//!                    └──────┬───────┘
//!                           │ leave_group()
//!                           ▼
//!                    ┌──────────────┐
//!                    │   Leaving    │ ─────────────────► cleanup
//!                    │              │   (commit received)
//!                    └──────────────┘
//! ```
//!
//! # Quick Start
//!
//! ```ignore
//! use de_mls::app::{User, GroupState, StateChangeHandler};
//! use de_mls::core::{GroupEventHandler, DefaultProvider};
//!
//! // Your handlers
//! struct MyHandler { /* ... */ }
//! impl GroupEventHandler for MyHandler { /* ... */ }
//! impl StateChangeHandler for MyHandler { /* ... */ }
//!
//! // Create user
//! let user = User::<DefaultProvider, _, _>::with_private_key(
//!     "0xprivate_key",
//!     consensus_service,
//!     event_handler,
//!     state_handler,
//! )?;
//!
//! // Create a group (as steward)
//! user.create_group("my-chat", true).await?;
//!
//! // Or join an existing group
//! user.create_group("other-chat", false).await?;
//! user.send_kp_message("other-chat").await?; // Send key package
//!
//! // Send messages
//! user.send_app_message("my-chat", b"Hello!".to_vec()).await?;
//!
//! // Process inbound (call from your transport receive loop)
//! user.process_inbound_packet(inbound_packet).await?;
//!
//! // Steward epoch (call periodically for steward groups)
//! user.start_steward_epoch("my-chat").await?;
//!
//! // Member epoch (call at epoch boundaries for non-steward groups)
//! let entered_waiting = user.start_member_epoch("other-chat").await?;
//! ```
//!
//! # Configuration
//!
//! Epoch duration can be configured per-user (default) or per-group:
//!
//! ```ignore
//! use de_mls::app::GroupConfig;
//! use std::time::Duration;
//!
//! // Custom default for all groups
//! let config = GroupConfig::with_epoch_duration(Duration::from_secs(60));
//! let user = User::with_private_key_and_config(key, consensus, handler, state_handler, config)?;
//!
//! // Or per-group
//! user.create_group_with_config("fast-chat", true, GroupConfig::with_epoch_duration(Duration::from_secs(10))).await?;
//! ```

mod scheduler;
mod state_machine;
mod user;

pub use scheduler::{IntervalScheduler, StewardScheduler, StewardSchedulerConfig};
pub use state_machine::{
    CommitTimeoutStatus, GroupConfig, GroupState, GroupStateMachine, StateChangeHandler,
};
pub use user::{User, UserError};
