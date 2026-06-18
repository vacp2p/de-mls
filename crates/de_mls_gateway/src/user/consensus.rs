//! [`ConsensusContext`] — the consensus storage + signer pair a `User`
//! creates once and mints every conversation's consensus service from.
//!
//! This is integrator wiring, not library API:
//! [`Conversation::create`](de_mls::Conversation::create) /
//! [`Conversation::join`](de_mls::Conversation::join) take a ready
//! [`ConsensusServiceFor`], and how services share storage is the
//! integrator's choice.

use hashgraph_like_consensus::storage::ConsensusStorage;

use de_mls::{ConsensusPlugin, ConsensusServiceFor};

/// Cap on concurrent consensus sessions per conversation scope; the
/// consensus library evicts the oldest session once the cap is reached.
const MAX_SESSIONS_PER_SCOPE: usize = 10;

/// Shared consensus backing for all of one integrator's conversations.
/// Per-conversation services mint from it via [`Self::build_service`].
pub struct ConsensusContext<P: ConsensusPlugin> {
    storage: P::ConsensusStorage,
    signer: P::Signer,
}

impl<P: ConsensusPlugin> ConsensusContext<P> {
    /// Build a fresh context around the integrator's `signer`; storage is
    /// created once here and shared by every service minted later.
    pub fn new(signer: P::Signer) -> Self {
        Self {
            storage: P::new_storage(),
            signer,
        }
    }

    /// Mint a per-conversation consensus service: shared scope-keyed
    /// storage and signer, but a private event bus — each conversation
    /// drains only its own outcomes.
    pub fn build_service(&self) -> ConsensusServiceFor<P> {
        ConsensusServiceFor::<P>::new_with_components(
            self.storage.clone(),
            P::new_event_bus(),
            self.signer.clone(),
            MAX_SESSIONS_PER_SCOPE,
        )
    }

    /// Drop a conversation's scope from the shared storage on leave.
    pub fn delete_scope(
        &self,
        scope: &P::Scope,
    ) -> Result<(), hashgraph_like_consensus::error::ConsensusError> {
        self.storage.delete_scope(scope)
    }
}

// Manual impl: `derive(Clone)` would require `P: Clone`, which plugin
// types don't (and shouldn't) provide.
impl<P: ConsensusPlugin> Clone for ConsensusContext<P> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            signer: self.signer.clone(),
        }
    }
}
