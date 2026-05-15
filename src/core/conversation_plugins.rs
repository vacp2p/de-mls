//! [`ConversationPluginsFactory`] trait — bundle of per-conversation plug-in
//! types plus the construction methods for each. The trait lives in
//! `core` so [`crate::core::ConversationHandle`] can take a single
//! bundle parameter rather than three independently-bounded type
//! parameters; integrators implement it once to swap any of the
//! per-conversation plug-ins. The convenience
//! [`crate::app::DefaultConversationPluginsFactory`] supplies an in-memory
//! build used by [`crate::app::User::with_private_key`].
//!
//! Key package generation lives on its own trait —
//! [`crate::core::KeyPackageProvider`] — because it is identity-bound,
//! not conversation-bound.

use crate::{
    core::{PeerScoringPlugin, ScoringConfig, StewardListConfig, StewardListPlugin},
    mls_crypto::{MlsError, MlsService},
};

/// Per-conversation plug-in bundle. One trait carries the three plug-in
/// types (`Mls`, `Scoring`, `StewardList`) plus the construction methods
/// for each. Identity is intentionally **not** part of this bundle — it
/// lives parallel to the conversation registry as `Arc<dyn Identity>` on
/// `User`.
pub trait ConversationPluginsFactory: Send + Sync + 'static {
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
}
