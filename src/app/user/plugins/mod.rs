//! User-level plugin types and bundle.
//!
//! Pieces (each in its own submodule):
//!
//! - [`bundle::UserPlugins`] — the struct holding all User-level plugin
//!   state: the per-conversation factory, the consensus context, the key
//!   package provider, and the three default configs cloned into new
//!   sessions.
//! - [`consensus_context::ConsensusContext`] — the consensus-plugin-side
//!   state (storage + signer) plus methods that build per-conv services
//!   and clean up scopes on leave.
//! - [`conversation_plugins::DefaultConversationPluginsFactory`] — reference
//!   impl of [`crate::core::ConversationPluginsFactory`] used by
//!   [`super::User::with_private_key`].
//! - [`key_package::DefaultKeyPackageProvider`] — reference impl of
//!   [`crate::core::KeyPackageProvider`].

mod bundle;
mod consensus_context;
mod conversation_plugins;
mod key_package;

pub(crate) use bundle::UserPlugins;
pub use consensus_context::ConsensusContext;
pub use conversation_plugins::{
    DefaultConversationPluginsFactory, DefaultMlsService, DefaultPeerScoring, DefaultStewardList,
};
pub use key_package::DefaultKeyPackageProvider;
