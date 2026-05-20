//! [`ConversationPluginsFactory`] trait — bundle of per-conversation plug-in
//! types plus the construction methods for each, *plus* the
//! identity-bound `generate_key_package` entry that joiners use before
//! any conversation exists. The trait lives in `core` so
//! [`crate::core::ConversationHandle`] can take a single bundle parameter
//! rather than several independently-bounded type parameters;
//! integrators implement it once to swap any of the per-conversation
//! plug-ins.

use crate::{
    core::{PeerScoringPlugin, ScoringConfig, StewardListConfig, StewardListPlugin},
    mls_crypto::{KeyPackageBytes, MlsError, MlsService},
};

/// Per-conversation plug-in bundle. One trait carries the three plug-in
/// types (`Mls`, `Scoring`, `StewardList`) plus the construction methods
/// for each. Identity is intentionally **not** part of this bundle — it
/// lives parallel to the conversation registry as `Arc<dyn Identity>` on
/// `User`.
pub trait ConversationPluginsFactory {
    type Mls: MlsService;
    type Scoring: PeerScoringPlugin;
    type StewardList: StewardListPlugin;

    /// Build an MLS service for a brand-new conversation as its sole creator.
    fn create_mls(&self, conversation_id: String) -> Result<Self::Mls, MlsError>;

    /// Try to build an MLS service from a serialized MLS welcome.
    /// Returns `Ok(None)` when the welcome isn't for us.
    fn welcome_mls(&self, welcome_bytes: &[u8]) -> Result<Option<Self::Mls>, MlsError>;

    /// Build a fresh peer-scoring plug-in for a new conversation runner.
    fn make_scoring(&self, config: &ScoringConfig) -> Self::Scoring;

    /// Build a fresh steward-list plug-in for a new conversation runner.
    /// Returns an empty plug-in; the lifecycle creator path bootstraps it
    /// via [`StewardListPlugin::install_list`].
    fn make_steward_list(
        &self,
        conversation_id: &[u8],
        config: StewardListConfig,
    ) -> Self::StewardList;

    /// Mint a single-use key package. Identity-bound (not
    /// conversation-bound): joiners publish a KP before any conversation
    /// exists, so this method takes no `conversation_id`.
    fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError>;
}
