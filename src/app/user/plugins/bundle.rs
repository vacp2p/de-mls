//! [`UserPlugins`] — bundle of all User-level plugin state on one struct
//! so the `User` definition surfaces registry + identity + transport at
//! top level and groups plugin concerns here.

use std::sync::Arc;

use crate::{
    app::ConsensusContext,
    core::{
        ConsensusPlugin, ConversationConfig, ConversationPluginsFactory, KeyPackageProvider,
        ScoringConfig, StewardListConfig,
    },
};

/// Bundle of all User-level plugin state. Held on [`super::super::User`] as
/// a single field so the User struct surfaces the registry + identity +
/// transport at top level and groups plugin concerns here.
pub struct UserPlugins<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    /// Builds per-conversation plug-in instances (MLS service, scoring,
    /// steward list). One factory per `User`; instances live on the
    /// corresponding `SessionRunner` after creation.
    pub conversation_plugins: Arc<CP>,
    /// Consensus-plugin state. Owns the shared storage handle + signer
    /// and mints per-conv services on demand.
    pub consensus: ConsensusContext<P>,
    /// Identity-bound key package generator. Independent of any
    /// conversation — joiners can mint KPs before `start_conversation`.
    pub key_package_provider: Arc<dyn KeyPackageProvider>,
    /// Seed config copied into newly-created `SessionRunner`s.
    pub default_conversation_config: ConversationConfig,
    /// Seed config for the per-conversation peer-scoring plug-in.
    pub default_scoring_config: ScoringConfig,
    /// Seed config for the per-conversation steward-list plug-in.
    pub default_steward_list_config: StewardListConfig,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Clone for UserPlugins<P, CP> {
    fn clone(&self) -> Self {
        Self {
            conversation_plugins: Arc::clone(&self.conversation_plugins),
            consensus: self.consensus.clone(),
            key_package_provider: Arc::clone(&self.key_package_provider),
            default_conversation_config: self.default_conversation_config.clone(),
            default_scoring_config: self.default_scoring_config.clone(),
            default_steward_list_config: self.default_steward_list_config.clone(),
        }
    }
}
