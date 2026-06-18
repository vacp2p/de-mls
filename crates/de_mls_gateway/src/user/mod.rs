//! [`User`] — multi-conversation facade over the de-mls library. One node
//! owns one `User`, which holds the per-conversation registry, the plugin
//! bundle, and the outbound transport. Per-conv protocol work lives on each
//! [`de_mls::Conversation`]; callers reach a session via
//! [`User::lookup_entry`].
//!
//! This is the reference integrator — the registry + routing + lifecycle
//! multiplexing a transport-bearing app needs on top of the transport-free
//! library. The library carries no transport routing of its own.
//!
//! Submodules:
//! - `state` — `User` struct, constructor, accessors, and consensus-
//!   scope cleanup. Construct via `User::new_with_plugins(&member_id,
//!   plugins, transport)`
//! - `lifecycle` — `start_conversation`, `leave_conversation` (registry CUD).
//! - `registry` — `lookup_entry`, `list_conversations`.
//! - `inbound` — `handle_inbound` / `receive_key_package` entry points,
//!   `finalize_self_leave` (registry-side completion of `LeaveConversation`).
//! - `error` — [`UserError`], the registry-level error wrapping the
//!   library's `ConversationError`.
//! - `consensus` — [`ConsensusContext`], the shared consensus storage +
//!   signer that mints each conversation's service.
//! - `plugins` — `UserPlugins<P, CP>` bundle.

mod consensus;
mod error;
mod inbound;
mod lifecycle;
mod lock;
mod plugins;
mod registry;
mod state;

pub use consensus::ConsensusContext;
pub use error::UserError;
pub use inbound::Inbound;
pub(crate) use lock::LockExt;
pub use plugins::UserPlugins;
pub use state::{ConversationEntry, ConversationLifecycle, User};
