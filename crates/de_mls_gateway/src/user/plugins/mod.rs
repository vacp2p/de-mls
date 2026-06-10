//! User-level plugin bundle.
//!
//! [`bundle::UserPlugins`] holds all User-level plugin state: the
//! per-conversation factory, the consensus context
//! ([`de_mls::app::ConsensusContext`]), and the three default configs
//! cloned into new sessions.

mod bundle;

pub use bundle::UserPlugins;
