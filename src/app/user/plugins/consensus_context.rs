//! [`ConsensusContext`] — User-level consensus-plugin state.
//!
//! Holds the shared storage handle + signer once on the User; mints fresh
//! per-conversation [`PluginConsensus`] services on demand and tears down a
//! conversation's scope on leave. Each per-conv service gets its own
//! private event bus (per upstream "service shape" recommendation).

use hashgraph_like_consensus::storage::ConsensusStorage;

use crate::core::{ConsensusPlugin, PluginConsensus};

pub struct ConsensusContext<P: ConsensusPlugin> {
    storage: P::ConsensusStorage,
    signer: P::Signer,
}

impl<P: ConsensusPlugin> ConsensusContext<P> {
    /// Build a fresh context. `P::new_storage` creates the shared backing
    /// once; `signer` is supplied by the integrator (typically wraps the
    /// user's wallet / account key).
    pub fn new(signer: P::Signer) -> Self {
        Self {
            storage: P::new_storage(),
            signer,
        }
    }

    /// Build a fresh per-conversation `ConsensusService`. Clones the shared
    /// storage handle so all per-conv services share one underlying
    /// persistence (scope-keyed); clones the signer; mints a fresh private
    /// event bus.
    pub fn build_service(&self) -> PluginConsensus<P> {
        PluginConsensus::<P>::new_with_components(
            self.storage.clone(),
            P::new_event_bus(),
            self.signer.clone(),
            10,
        )
    }

    /// Drop a conversation's scope from the shared consensus storage.
    /// Called on conversation leave.
    pub async fn delete_scope(
        &self,
        scope: &P::Scope,
    ) -> Result<(), hashgraph_like_consensus::error::ConsensusError> {
        self.storage.delete_scope(scope).await
    }
}

impl<P: ConsensusPlugin> Clone for ConsensusContext<P> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            signer: self.signer.clone(),
        }
    }
}
