//! User-level plugin bundle.
//!
//! [`UserPlugins`] holds all User-level plugin state: the per-conversation
//! factory, the [`ConsensusContext`], and the three default configs cloned
//! into new conversations. Grouping these here keeps the `User` definition
//! surfacing registry + transport at top level.

use de_mls::{ConsensusPlugin, ConversationConfig, ScoringConfig, StewardListConfig};

use crate::mls::DefaultConversationPluginsFactory;
use crate::user::ConsensusContext;

/// Bundle of all User-level plugin state. One factory plus its three
/// seed configs, owned outright.
pub struct UserPlugins<P: ConsensusPlugin> {
    /// Builds per-conversation plug-in instances (scoring, steward list),
    /// supplies the OpenMLS provider + credential the library seeds the MLS
    /// service from, and mints key packages for joiners.
    pub conversation_plugins: DefaultConversationPluginsFactory,
    /// Consensus-plugin state. Owns the shared storage + signer
    /// and mints per-conv services on demand.
    pub consensus: ConsensusContext<P>,
    /// Seed config copied into newly-created `Conversation`s.
    pub default_conversation_config: ConversationConfig,
    /// Seed config for the per-conversation peer-scoring plug-in.
    pub default_scoring_config: ScoringConfig,
    /// Seed config for the per-conversation steward-list plug-in.
    pub default_steward_list_config: StewardListConfig,
}
