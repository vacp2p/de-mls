//! User-level plugin types and bundle.
//!
//! - [`bundle::UserPlugins`] — the struct holding all User-level plugin
//!   state: the per-conversation factory, the consensus context, the key
//!   package provider, and the three default configs cloned into new
//!   sessions.
//! - [`consensus_context::ConsensusContext`] — the consensus-plugin-side
//!   state (storage + signer) plus methods that build per-conv services
//!   and clean up scopes on leave.

mod bundle;
mod consensus_context;

pub use bundle::UserPlugins;
pub use consensus_context::ConsensusContext;
