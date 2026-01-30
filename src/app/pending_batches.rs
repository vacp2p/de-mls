//! Storage for pending batch proposals.
//!
//! Non-steward nodes may receive batch proposals before consensus is reached.
//! This module provides storage for those proposals until they can be processed.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::protos::de_mls::messages::v1::BatchProposalsMessage;

/// Storage for pending batch proposals.
///
/// When a non-steward node receives a batch proposals message before
/// reaching consensus, it stores the message here for later processing.
#[derive(Clone, Default)]
pub struct PendingBatches {
    inner: Arc<RwLock<HashMap<String, BatchProposalsMessage>>>,
}

impl PendingBatches {
    /// Create a new empty pending batches storage.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Store a batch proposals message for a group.
    ///
    /// # Arguments
    /// * `group` - The group name
    /// * `batch` - The batch proposals message
    pub async fn store(&self, group: &str, batch: BatchProposalsMessage) {
        self.inner.write().await.insert(group.to_string(), batch);
    }

    /// Take (remove and return) the stored batch for a group.
    ///
    /// # Arguments
    /// * `group` - The group name
    ///
    /// # Returns
    /// The stored batch if present, None otherwise.
    pub async fn take(&self, group: &str) -> Option<BatchProposalsMessage> {
        self.inner.write().await.remove(group)
    }

    /// Check if there's a stored batch for a group.
    ///
    /// # Arguments
    /// * `group` - The group name
    pub async fn contains(&self, group: &str) -> bool {
        self.inner.read().await.contains_key(group)
    }

    /// Clear all stored batches.
    pub async fn clear(&self) {
        self.inner.write().await.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pending_batches() {
        let pending = PendingBatches::new();

        let batch = BatchProposalsMessage {
            group_name: b"test-group".to_vec(),
            mls_proposals: vec![],
            commit_message: vec![],
        };

        // Store
        pending.store("test-group", batch.clone()).await;
        assert!(pending.contains("test-group").await);

        // Take
        let taken = pending.take("test-group").await;
        assert!(taken.is_some());
        assert!(!pending.contains("test-group").await);

        // Take again returns None
        let taken_again = pending.take("test-group").await;
        assert!(taken_again.is_none());
    }
}
