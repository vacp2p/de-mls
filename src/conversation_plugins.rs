//! [`ConversationPlugins`] trait — types-only bundle of the three
//! per-conversation plug-in types.
//!
//! Key-package generation is **not** here: the protocol layer never mints key
//! packages (a joiner publishes one out of band before any conversation
//! exists), so it is the integrator's concern, not part of this contract.
//!
//! The integrator builds the plug-in *instances* and passes them in via
//! [`crate::ConversationDeps`]; this trait only names their types.

use crate::{PeerScoringPlugin, StewardListPlugin, mls_crypto::MlsService};

/// Per-conversation plug-in bundle. Names the three plug-in types
/// (`Mls`, `Scoring`, `StewardList`) the construction API is generic over.
pub trait ConversationPlugins {
    type Mls: MlsService;
    type Scoring: PeerScoringPlugin;
    type StewardList: StewardListPlugin;
}
