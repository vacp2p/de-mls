//! [`User`] — multi-conversation facade over core. One node owns one `User`,
//! which holds the per-conversation registry, the key-package
//! provider, the consensus context, and the outbound transport. Per-conv
//! protocol work lives on each [`crate::app::SessionRunner`]; callers reach
//! a session via [`User::lookup_entry`].
//!
//! Submodules:
//! - [`state`] — `User` struct, constructor, accessors, and consensus-
//!   scope cleanup. Construct via `User::new_with_plugins(&member_id,
//!   plugins, transport)`
//! - [`lifecycle`] — `start_conversation`, `leave_conversation` (registry CUD).
//! - [`registry`] — `lookup_entry`, `list_conversations`,
//!   `subscribe_conversations`.
//! - [`inbound`] — `handle_inbound` / `receive_key_package` entry points,
//!   `finalize_self_leave` (registry-side completion of `LeaveConversation`).
//! - [`plugins`] — `UserPlugins<P, CP>` bundle + `ConsensusContext<P>`.
//!   Reference per-conversation plug-in impls live in [`crate::defaults`].

mod inbound;
mod lifecycle;
mod lock;
mod plugins;
mod registry;
mod state;

pub use inbound::Inbound;
pub(crate) use lock::LockExt;
pub use plugins::{ConsensusContext, UserPlugins};
pub use state::{SessionEntry, User};
