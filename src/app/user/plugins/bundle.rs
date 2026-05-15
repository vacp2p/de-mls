//! [`UserPlugins`] — bundle of all User-level plugin state on one struct
//! so the `User` definition surfaces registry + identity + transport at
//! top level and groups plugin concerns here.

use std::sync::Arc;

use crate::{
    core::{
        ConsensusPlugin, ConversationConfig, ConversationPluginsFactory, KeyPackageProvider,
        ScoringConfig, StewardListConfig,
    },
    mls_crypto::{MemoryDeMlsStorage, MlsCredentials, MlsError},
};

use super::{ConsensusContext, DefaultConversationPluginsFactory, DefaultKeyPackageProvider};

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

impl UserPlugins<crate::core::DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
    /// Build the default plug-in bundle from a wallet `PrivateKeySigner` and
    /// the derived identity. Used by [`super::super::User::with_private_key`].
    pub(crate) fn default_for_wallet(
        identity: &dyn crate::identity::Identity,
        signer: alloy::signers::local::PrivateKeySigner,
        default_conversation_config: ConversationConfig,
    ) -> Result<Self, MlsError> {
        let credentials = Arc::new(MlsCredentials::from_identity(identity)?);
        let storage = Arc::new(MemoryDeMlsStorage::new());
        let conversation_plugins = Arc::new(DefaultConversationPluginsFactory {
            storage: Arc::clone(&storage),
            credentials: Arc::clone(&credentials),
        });
        let key_package_provider: Arc<dyn KeyPackageProvider> =
            Arc::new(DefaultKeyPackageProvider {
                storage,
                credentials,
            });
        let consensus_signer =
            hashgraph_like_consensus::signing::EthereumConsensusSigner::new(signer);
        let consensus =
            ConsensusContext::<crate::core::DefaultConsensusPlugin>::new(consensus_signer);
        Ok(Self {
            conversation_plugins,
            consensus,
            key_package_provider,
            default_conversation_config,
            default_scoring_config: ScoringConfig::default(),
            default_steward_list_config: StewardListConfig::default(),
        })
    }
}
