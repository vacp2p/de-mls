//! User-level plugin bundle.
//!
//! [`UserPlugins`] holds all User-level plugin state: the per-conversation
//! factory, the consensus context ([`de_mls::session::ConsensusContext`]), and
//! the three default configs cloned into new sessions. Grouping these here
//! keeps the `User` definition surfacing registry + transport at top level.

use de_mls::{
    core::{
        ConsensusPlugin, ConversationConfig, ConversationPluginsFactory, ScoringConfig,
        StewardListConfig,
    },
    session::ConsensusContext,
};

/// Bundle of all User-level plugin state. One factory plus its three
/// seed configs, owned outright.
pub struct UserPlugins<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Builds per-conversation plug-in instances (MLS service, scoring,
    /// steward list) and generate key packages for joiners.
    pub conversation_plugins: CP,
    /// Consensus-plugin state. Owns the shared storage + signer
    /// and mints per-conv services on demand.
    pub consensus: ConsensusContext<P>,
    /// Seed config copied into newly-created `SessionRunner`s.
    pub default_conversation_config: ConversationConfig,
    /// Seed config for the per-conversation peer-scoring plug-in.
    pub default_scoring_config: ScoringConfig,
    /// Seed config for the per-conversation steward-list plug-in.
    pub default_steward_list_config: StewardListConfig,
}
