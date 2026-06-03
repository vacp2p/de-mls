//! [`UserPlugins`] — bundle of all User-level plugin state on one struct
//! so the `User` definition surfaces registry, transport at
//! top level and groups plugin concerns here.

use crate::{
    app::ConsensusContext,
    core::{
        ConsensusPlugin, ConversationConfig, ConversationPluginsFactory, ScoringConfig,
        StewardListConfig,
    },
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
